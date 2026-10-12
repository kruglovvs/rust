//! SD-card A/V recorder: single-clock alignment + segment rollover
//! (plan `esp32s3-av-stream-and-sd-recording.md`, Todo 10).
//!
//! # Plan contract (quoted from the todo)
//!
//! * **A single timestamp authority drives both tracks.** Video PTS comes
//!   from the camera frame's capture time, audio PTS from the mic's sample
//!   counter, both expressed in **microseconds on one monotonic clock** so
//!   the drift stays bounded. Callers pass `pts_us` into
//!   [`Recorder::write_video_frame`] / [`Recorder::write_audio_samples`];
//!   on the device both derive from the same `Instant`/`esp_timer` epoch
//!   (Todo 20 wires the capture tasks; this module never reads a clock
//!   itself, which is also what makes the whole pipeline host-testable).
//! * **Drop, NEVER duplicate, video frames when the camera falls behind.**
//!   An AVI is CFR on both tracks — video duration = `frames × 1/fps`,
//!   audio = `samples / 16000`, both derived from *counts*, never from
//!   wall-clock. A stalled camera must not stretch the take: a frame whose
//!   clock slot has already been written is **dropped and counted**
//!   ([`VideoOutcome::Dropped`]), never re-timed, never duplicated, and
//!   never allowed to rewind the PTS. The invariant the harness proves:
//!   `frames_v × frame_period ≈ samples_a / 16000` for a healthy take, and
//!   honest short-video accounting (not frozen-frame padding) for a stall.
//! * **Roll to a NEW `.avi` file** — close the current segment cleanly via
//!   `Muxer::finish()`, increment the index, open the next — on any of
//!   three triggers (checked before every write):
//!   1. **size bound** (default 256 MiB — the primary trigger at defaults;
//!      at the plan's corrected 0.09–0.15 MB/s a 256 MiB segment lasts
//!      ~31–47 min),
//!   2. **wall-clock bound** (default **60 min** — the *safety net* for a
//!      stalled-rate sensor; 60 min sits above the size bound's ~31–47 min
//!      so it never pre-empts it at defaults, which is exactly why the
//!      plan raised it from the original 30 min),
//!   3. **storage error** — a write error finishes the current segment
//!      cleanly, logs it, then continues or stops per [`ErrorPolicy`]
//!      (default [`ErrorPolicy::Roll`]: roll to the next segment and retry
//!      the packet once; if the next segment cannot even be *opened* —
//!      full volume, dead card — the recorder stops with a typed
//!      diagnostic instead of looping or writing past the 2 GiB `Avi10`
//!      ceiling; [`MuxerError::SegmentFull`] is the pre-checked ceiling
//!      signal from Todo 9 and always rolls).
//! * **Segment naming: monotonic, zero-padded, never overwrite.**
//!   `REC_0001.avi`, `REC_0002.avi`, … — creation uses `create_new`
//!   semantics and existing names are skipped, so a re-run or a resumed
//!   session never clobbers a previous take (asserted in the unit tests
//!   by pre-creating a file).
//! * **A/V assertions the host proves on the recorded AVI** (Todo 10
//!   acceptance): `|duration_video − duration_audio| ≤ 250 ms` and
//!   audio/video start-time delta ≤ 250 ms on the file; rollover produces
//!   multiple separately-playable `.avi` files, each ffprobe-valid with
//!   two streams, with **no OpenDML `AVIX` chunk anywhere** (IS-5 — one
//!   file that merely contains rolled segments fails the requirement;
//!   every segment here is an independent `AviKind::Avi10` RIFF).
//!
//! # Architectural rule (enables the host proof)
//!
//! This module is **pure `std` + [`crate::muxer`] only** — no `esp_idf_*`
//! import, ever (same rule as `muxer.rs`). Storage (VFS paths over the
//! mounted SD card), the camera and the mic enter **only at the call
//! sites** in later wiring (Todo 20 / full-system): the capture tasks
//! produce `(pts_us, bytes)` and call in; the [`SegmentFactory`] handed to
//! the recorder decides where segment files live ([`DirFactory`] over a
//! plain directory works on the host and over ESP-IDF's FAT VFS alike).
//! That is what lets `harness/` `#[path]`-include this file and run the
//! rollover/drift/naming proofs with real `ffprobe` on the host.
//!
//! # CFR accounting (what each PTS means)
//!
//! * **Video**: `slot(pts_us) = (pts_us − segment_base_us) × fps /
//!   1_000_000` is the frame's position on the shared clock. A frame is
//!   written iff `slot` is strictly greater than every slot written in
//!   this segment; otherwise it is a late/stale delivery and is dropped.
//!   The muxer sees `pts_frame = slot` (clock-derived, gaps allowed — a
//!   stall leaves an honest hole in the timeline rather than a stretched
//!   or duplicated frame).
//! * **Audio**: `pts_sample` is the running count of sample frames
//!   written in this segment — the mic's sample counter, which *is* time
//!   at 16 kHz. `pts_us` is used for the wall-clock bound and the
//!   start-time-delta measurement; the sample position stays contiguous
//!   (a lost mic chunk shortens the take, it never fabricates silence —
//!   mirroring the video drop philosophy; `mic.rs` drops timed-out chunks
//!   the same way).
//!
//! Hardware rows (5-minute on-device take, real SD) are PENDING_NO_BOARD;
//! host proofs run in `harness` (`task10_recorder` bin + test).

use std::fmt;
use std::io;
use std::path::PathBuf;

use oxideav_core::WriteSeek;

use crate::muxer::{
    AudioTrack, AviMuxer, Muxer, MuxerError, MockMuxer, VideoTrack, AVI10_LIMIT_BYTES,
};

// ---------------------------------------------------------------------------
// Defaults (plan Todo 10 "Do")
// ---------------------------------------------------------------------------

/// Default segment size bound: **256 MiB**. The primary rollover trigger
/// at defaults — at the corrected 0.09–0.15 MB/s recording bitrate a
/// segment lasts ~31–47 min (plan Todo 10 References).
pub const DEFAULT_SEGMENT_SIZE_BYTES: u64 = 256 * 1024 * 1024;

/// Default segment wall-clock bound: **60 minutes**. The plan explicitly
/// raised this from 30 min to 60 min so the wall-clock bound is a *safety
/// net* against a stalled-rate sensor and never pre-empts the size bound
/// (a 30-min bound fired first every time at defaults, making the size
/// bound dead code).
pub const DEFAULT_SEGMENT_WALLCLOCK_US: u64 = 60 * 60 * 1_000_000;

/// Zero-pad width for segment names: `REC_0001.avi`. Indices ≥ 10000 are
/// *not* truncated — `{:04}` grows — so naming stays monotonic forever.
pub const SEGMENT_INDEX_WIDTH: usize = 4;

/// Skip-scan cap when probing for a free segment index: if this many
/// consecutive names are taken/unopenable the storage is effectively
/// unusable and we stop with a typed diagnostic rather than scanning
/// forever.
pub const MAX_SEGMENT_INDEX_PROBES: u32 = 10_000;

/// `REC_0001.avi` for index 1. Pure function so tests (and Todo 20's
/// logging) can name files without opening a recorder.
pub fn segment_file_name(index: u32) -> String {
    format!("REC_{index:0width$}.avi", width = SEGMENT_INDEX_WIDTH)
}

// ---------------------------------------------------------------------------
// Errors (no thiserror — house style: hand-written Display + Error)
// ---------------------------------------------------------------------------

/// Typed failures from the recording path. Every variant names the segment
/// it happened on so a device log line is self-describing.
#[derive(Debug)]
pub enum RecorderError {
    /// Opening a new segment file failed (card full, card removed, VFS
    /// error). If this happens *mid-take* while rolling, the previous
    /// segment was already closed cleanly — only the continuation is lost.
    SegmentOpen { index: u32, source: io::Error },
    /// The free-index scan hit [`MAX_SEGMENT_INDEX_PROBES`] consecutive
    /// taken names — stop rather than loop.
    IndexExhausted { from: u32 },
    /// Constructing the segment muxer failed (degenerate track config —
    /// should be impossible after `RecorderConfig::new` validation).
    Muxer(MuxerError),
    /// A packet write failed on segment `index` after all roll retries.
    /// The segment was closed cleanly (best effort) before this error
    /// surfaced — never a silently truncated file.
    Write { index: u32, source: Box<MuxerError> },
    /// The recorder was stopped by [`ErrorPolicy::Stop`] (or a terminal
    /// failure); the string is the error that caused it.
    Stopped(String),
    /// `finish()` was already called; the recorder is closed.
    Finished,
    /// Config rejected before any file was created.
    InvalidConfig(String),
}

impl fmt::Display for RecorderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecorderError::SegmentOpen { index, source } => write!(
                f,
                "recorder: cannot open segment {} ({}): {source}",
                segment_file_name(*index),
                index
            ),
            RecorderError::IndexExhausted { from } => write!(
                f,
                "recorder: no free segment name after {MAX_SEGMENT_INDEX_PROBES} probes starting at index {from}"
            ),
            RecorderError::Muxer(e) => write!(f, "recorder: segment muxer: {e}"),
            RecorderError::Write { index, source } => write!(
                f,
                "recorder: write failed on segment {} ({}): {source}",
                segment_file_name(*index),
                index
            ),
            RecorderError::Stopped(reason) => write!(f, "recorder: stopped: {reason}"),
            RecorderError::Finished => write!(f, "recorder: already finished"),
            RecorderError::InvalidConfig(msg) => write!(f, "recorder config: {msg}"),
        }
    }
}

impl std::error::Error for RecorderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RecorderError::SegmentOpen { source, .. } => Some(source),
            RecorderError::Muxer(e) => Some(e),
            RecorderError::Write { source, .. } => Some(source),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Segment storage factory (the only place "where do files live" is decided)
// ---------------------------------------------------------------------------

/// Creates segment files for the recorder. Implementations must honour
/// **no-overwrite**: `create` fails with [`io::ErrorKind::AlreadyExists`]
/// if `name` exists. The recorder probes with [`SegmentFactory::exists`]
/// *and* treats `AlreadyExists` from `create` as "skip to the next index",
/// so naming stays monotonic even under a race.
pub trait SegmentFactory {
    /// Does a file with this segment name already exist?
    fn exists(&self, name: &str) -> io::Result<bool>;
    /// Create a **new** file (never truncate/overwrite) as the segment
    /// sink. Seekable by muxer contract — the SD card through VFS is, and
    /// so are host files.
    fn create(&self, name: &str) -> io::Result<Box<dyn WriteSeek>>;
}

/// [`SegmentFactory`] over a plain directory: works on the host and over
/// ESP-IDF's FAT VFS (`/sdcard/...`) unchanged — pure `std::fs`, no
/// storage-module coupling. The mount itself stays Todo 7/20's job; this
/// only names files inside an already-mounted directory.
pub struct DirFactory {
    dir: PathBuf,
}

impl DirFactory {
    /// Segment files are created inside `dir` (created if missing).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The directory segments are created in.
    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }
}

impl SegmentFactory for DirFactory {
    fn exists(&self, name: &str) -> io::Result<bool> {
        self.dir.join(name).try_exists()
    }

    fn create(&self, name: &str) -> io::Result<Box<dyn WriteSeek>> {
        std::fs::create_dir_all(&self.dir)?;
        // create_new = O_CREAT|O_EXCL: AlreadyExists on collision, never a
        // silent overwrite of a previous take.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.dir.join(name))?;
        Ok(Box::new(file))
    }
}

// ---------------------------------------------------------------------------
// Policy + configuration
// ---------------------------------------------------------------------------

/// What to do when a packet **write** fails with a non-`SegmentFull`
/// error (typically [`MuxerError::Io`] — card pulled, volume full).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorPolicy {
    /// Close the current segment cleanly, log, roll to the next segment
    /// and retry the packet once. If the next segment cannot be opened
    /// (full volume, dead card) the recorder stops with a typed
    /// diagnostic. **Default** — a multi-hour session survives a single
    /// bad sector; a dead card still stops loudly (IS-5).
    Roll,
    /// Close the current segment cleanly, log, and stop the recorder.
    /// Subsequent writes return [`RecorderError::Stopped`].
    Stop,
}

/// Which `Muxer` implementation a segment gets. [`AviKind::Avi10`] via
/// [`AviMuxer`] is the production choice; [`MockMuxer`] exists so the
/// recorder's *logic* (naming, triggers, drop accounting) is unit-testable
/// with zero files and zero codec involvement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MuxerKind {
    /// Real AVI 1.0 files through `AviMuxer` (never OpenDML — plan D-2).
    Avi,
    /// In-RAM packet log (logic tests only; produces no playable file).
    Mock,
}

/// Recorder configuration. Built with [`RecorderConfig::new`] (validates
/// tracks, fills plan defaults) then adjusted field-wise; re-validated by
/// [`Recorder::new`].
#[derive(Clone, Debug)]
pub struct RecorderConfig {
    /// Video track handed to every segment muxer (QVGA MJPEG at the
    /// board's 10 fps operating point in production).
    pub video: VideoTrack,
    /// Audio track handed to every segment muxer (16 kHz mono PCM).
    pub audio: AudioTrack,
    /// Rollover trigger 1: roll once the current segment's committed bytes
    /// reach this bound (checked before the next write; the segment may
    /// overshoot by at most one packet). Default
    /// [`DEFAULT_SEGMENT_SIZE_BYTES`].
    pub segment_size_bytes: u64,
    /// Rollover trigger 2: roll once `pts_us − segment_base_us` reaches
    /// this bound. Default [`DEFAULT_SEGMENT_WALLCLOCK_US`] (60 min —
    /// safety net *above* the size bound's ~31–47 min at defaults).
    pub segment_wallclock_us: u64,
    /// Avi10 ceiling handed to `AviMuxer::new_with_limit`. Tests shrink it
    /// to prove the [`MuxerError::SegmentFull`] roll path without writing
    /// 2 GiB; production keeps [`AVI10_LIMIT_BYTES`].
    pub avi10_limit_bytes: u64,
    /// Segment muxer implementation. Default [`MuxerKind::Avi`].
    pub muxer_kind: MuxerKind,
    /// Rollover trigger 3 policy (write errors). Default
    /// [`ErrorPolicy::Roll`].
    pub error_policy: ErrorPolicy,
    /// First index probed for the first segment. Default 1 → `REC_0001.avi`.
    pub first_segment_index: u32,
}

impl RecorderConfig {
    /// Config with plan defaults for the given (validated) tracks.
    pub fn new(video: VideoTrack, audio: AudioTrack) -> Self {
        Self {
            video,
            audio,
            segment_size_bytes: DEFAULT_SEGMENT_SIZE_BYTES,
            segment_wallclock_us: DEFAULT_SEGMENT_WALLCLOCK_US,
            avi10_limit_bytes: AVI10_LIMIT_BYTES,
            muxer_kind: MuxerKind::Avi,
            error_policy: ErrorPolicy::Roll,
            first_segment_index: 1,
        }
    }

    fn validate(&self) -> Result<(), RecorderError> {
        // Re-check what `VideoTrack::new`/`AudioTrack::new` check — the
        // fields are public and callers can build the structs literally.
        if self.video.width == 0 || self.video.height == 0 || self.video.frame_rate == 0 {
            return Err(RecorderError::InvalidConfig(
                "video track width/height/frame_rate must be non-zero".into(),
            ));
        }
        if self.audio.sample_rate == 0 || self.audio.channels == 0 {
            return Err(RecorderError::InvalidConfig(
                "audio track sample_rate/channels must be non-zero".into(),
            ));
        }
        if self.segment_size_bytes == 0 {
            return Err(RecorderError::InvalidConfig(
                "segment_size_bytes must be non-zero".into(),
            ));
        }
        if self.segment_wallclock_us == 0 {
            return Err(RecorderError::InvalidConfig(
                "segment_wallclock_us must be non-zero".into(),
            ));
        }
        if self.avi10_limit_bytes < 4096 {
            return Err(RecorderError::InvalidConfig(
                "avi10_limit_bytes below 4 KiB cannot hold an AVI header".into(),
            ));
        }
        if self.first_segment_index == 0 {
            return Err(RecorderError::InvalidConfig(
                "first_segment_index must be ≥ 1 (0 would be REC_0000.avi)".into(),
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Outcomes + stats
// ---------------------------------------------------------------------------

/// What happened to one offered video frame. The drop accounting this
/// enum feeds is exactly the "drop, never duplicate" proof the harness
/// asserts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoOutcome {
    /// Frame written to the current segment at clock slot `pts_frame`
    /// (segment-relative, may contain gaps after a stall — gaps are
    /// honest; padding would be the bug).
    Written { pts_frame: u64 },
    /// Frame arrived with a clock slot already covered by an earlier
    /// write in this segment (camera fell behind / burst delivery). It
    /// was **dropped, not duplicated** — no bytes were written and the
    /// slot was not re-timed.
    Dropped { slot: u64 },
}

/// Why a segment was closed (except the final [`Recorder::finish`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RollReason {
    /// Trigger 1: size bound reached.
    Size,
    /// Trigger 2: wall-clock bound reached.
    WallClock,
    /// `MuxerError::SegmentFull` — the pre-checked 2 GiB Avi10 ceiling.
    SegmentFull,
    /// Trigger 3: a write failed and the policy rolled.
    WriteError,
}

/// Cumulative recording statistics. Also the host-test oracle: frame/sample
/// accounting and the A/V start-time delta are read straight off this.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecorderStats {
    /// Segments successfully opened (including the current one).
    pub segments_opened: u32,
    /// Segments closed with a clean `Muxer::finish()`.
    pub segments_completed: u32,
    /// Segments whose `Muxer::finish()` failed (logged; the file may be
    /// truncated — never silent, hence counted).
    pub segment_finish_errors: u32,
    /// Rolls caused by the size bound.
    pub rolls_size: u32,
    /// Rolls caused by the wall-clock bound.
    pub rolls_wallclock: u32,
    /// Rolls caused by `MuxerError::SegmentFull` (2 GiB Avi10 ceiling).
    pub rolls_segment_full: u32,
    /// Rolls caused by write errors (policy [`ErrorPolicy::Roll`]).
    pub rolls_error: u32,
    /// Video frames offered to the recorder (healthy + late).
    pub video_offered: u64,
    /// Video frames actually written.
    pub video_written: u64,
    /// Video frames dropped as late/stale — never duplicated.
    pub video_dropped: u64,
    /// Audio chunks written.
    pub audio_chunks_written: u64,
    /// PCM sample frames written (the audio duration numerator).
    pub audio_samples_written: u64,
    /// Index of the current (or last) segment.
    pub current_segment_index: u32,
    /// Committed bytes of the last *closed* segment.
    pub last_segment_bytes: u64,
    /// `pts_us` of the first video frame of the take (single-clock epoch
    /// anchor for the start-delta measurement).
    pub first_video_pts_us: Option<u64>,
    /// `pts_us` of the first audio chunk of the take.
    pub first_audio_pts_us: Option<u64>,
    /// True once [`Recorder::finish`] completed.
    pub finished: bool,
    /// True once a terminal failure stopped the recorder.
    pub stopped: bool,
}

impl RecorderStats {
    /// `|first_video − first_audio|` on the shared clock, in µs — the
    /// plan's "A/V start-time delta ≤ 250 ms" input, measured, not
    /// assumed. `None` until both tracks have delivered a packet.
    pub fn av_start_delta_us(&self) -> Option<u64> {
        match (self.first_video_pts_us, self.first_audio_pts_us) {
            (Some(v), Some(a)) => Some(v.abs_diff(a)),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Recorder
// ---------------------------------------------------------------------------

/// One packet offered to the segment muxer. Exists so the roll-retry path
/// can re-derive PTS from the *live* segment state after a mid-write roll
/// (see [`Recorder::write_packet_with_rolls`]).
enum PacketOut<'a> {
    Video { jpeg: &'a [u8] },
    Audio { pcm: &'a [u8] },
}

/// The recorder: accepts timestamped frames/samples on one clock, writes
/// them into the current AVI segment through the [`Muxer`] trait, and rolls
/// segments per [`RecorderConfig`].
///
/// Not `Clone`/`Sync` by design — one take, one recorder, driven from the
/// capture side (the same threading model as the reference
/// `esp-cpp/camera-streamer` design: one writer task).
pub struct Recorder {
    config: RecorderConfig,
    factory: Box<dyn SegmentFactory>,
    muxer: Option<Box<dyn Muxer>>,
    /// pts_us anchor of the current segment (set by the first write that
    /// opens it).
    segment_base_us: Option<u64>,
    /// Highest video slot written in the current segment (drop gate).
    last_video_slot: Option<u64>,
    /// Running audio sample-frame count in the current segment (the mic
    /// sample counter — audio PTS).
    audio_samples_in_segment: u64,
    /// Next index the free-name scan will start from (monotonic, skips
    /// existing files).
    next_index: u32,
    /// Set after the first write so `finish()` has a segment to close
    /// even if the caller offers no more packets.
    segment_name: String,
    stats: RecorderStats,
}

impl Recorder {
    /// Open the recorder: validates the config, finds the first free
    /// segment name (never overwriting), creates the segment file and its
    /// muxer. The first packet's `pts_us` becomes the segment's clock
    /// base, so opening eagerly costs nothing on the timeline.
    pub fn new(
        config: RecorderConfig,
        factory: impl SegmentFactory + 'static,
    ) -> Result<Self, RecorderError> {
        config.validate()?;
        let mut rec = Self {
            config,
            factory: Box::new(factory),
            muxer: None,
            segment_base_us: None,
            last_video_slot: None,
            audio_samples_in_segment: 0,
            next_index: 1, // overwritten below from config
            segment_name: String::new(),
            stats: RecorderStats::default(),
        };
        rec.next_index = rec.config.first_segment_index;
        rec.open_next_segment(None)?;
        Ok(rec)
    }

    /// Segment-relative video slot for a capture timestamp: the single
    /// clock arithmetic the whole drop/CFR scheme rests on. `pub` so the
    /// harness can assert the exact rounding.
    pub fn video_slot(pts_us: u64, segment_base_us: u64, frame_rate: u32) -> u64 {
        let delta = pts_us.saturating_sub(segment_base_us);
        ((delta as u128) * frame_rate as u128 / 1_000_000) as u64
    }

    /// Offer one JPEG frame captured at `pts_us` on the shared clock.
    ///
    /// Returns [`VideoOutcome::Dropped`] (typed, counted, no bytes) when
    /// the frame's slot is not strictly ahead of everything written in
    /// this segment — the drop-never-duplicate backpressure. Errors are
    /// terminal-or-rolled per [`ErrorPolicy`]; after `finish()` or a
    /// stop, [`RecorderError::Finished`] / [`RecorderError::Stopped`].
    pub fn write_video_frame(
        &mut self,
        pts_us: u64,
        jpeg: &[u8],
    ) -> Result<VideoOutcome, RecorderError> {
        self.ensure_writable()?;
        if jpeg.is_empty() {
            return Err(RecorderError::Write {
                index: self.stats.current_segment_index,
                source: Box::new(MuxerError::InvalidPacket("empty JPEG frame".into())),
            });
        }
        self.maybe_roll_for_bounds(pts_us)?;

        // Establish the segment's clock base on its first write (a roll
        // resets it; the base is always the first delivered pts).
        let base = *self.segment_base_us.get_or_insert(pts_us);
        let slot = Self::video_slot(pts_us, base, self.config.video.frame_rate);
        if let Some(last) = self.last_video_slot {
            if slot <= last {
                // Camera fell behind / burst delivery: this slot's moment
                // has passed. DROP — never duplicate, never re-time.
                self.stats.video_offered += 1;
                self.stats.video_dropped += 1;
                return Ok(VideoOutcome::Dropped { slot });
            }
        }

        self.stats.video_offered += 1;
        let written_slot =
            self.write_packet_with_rolls(pts_us, PacketOut::Video { jpeg })?;
        self.last_video_slot = Some(written_slot);
        self.stats.video_written += 1;
        if self.stats.first_video_pts_us.is_none() {
            self.stats.first_video_pts_us = Some(pts_us);
        }
        Ok(VideoOutcome::Written { pts_frame: written_slot })
    }

    /// Offer one PCM chunk captured at `pts_us` on the shared clock.
    /// `samples` is mono `i16` (or interleaved for `channels > 1`; must be
    /// a whole number of sample frames). Audio PTS is the running sample
    /// counter — contiguous, never fabricated, never dropped silently
    /// (the caller's loss shortens the take; see module docs).
    pub fn write_audio_samples(&mut self, pts_us: u64, samples: &[i16]) -> Result<(), RecorderError> {
        self.ensure_writable()?;
        if samples.is_empty() {
            return Ok(());
        }
        let channels = self.config.audio.channels as usize;
        if samples.len() % channels != 0 {
            return Err(RecorderError::Write {
                index: self.stats.current_segment_index,
                source: Box::new(MuxerError::InvalidPacket(format!(
                    "{} samples not a whole number of {}-channel sample frames",
                    samples.len(),
                    channels
                ))),
            });
        }
        self.maybe_roll_for_bounds(pts_us)?;
        // Anchor the segment clock if this chunk opens the segment (an
        // audio-first start is legitimate — the mic's 20 ms chunks land
        // before the camera's first 100 ms frame).
        self.segment_base_us.get_or_insert(pts_us);

        let frames = (samples.len() / channels) as u64;
        let mut pcm = Vec::with_capacity(samples.len() * 2);
        for s in samples {
            pcm.extend_from_slice(&s.to_le_bytes());
        }
        self.write_packet_with_rolls(pts_us, PacketOut::Audio { pcm: &pcm })?;
        self.audio_samples_in_segment += frames;
        self.stats.audio_chunks_written += 1;
        self.stats.audio_samples_written += frames;
        if self.stats.first_audio_pts_us.is_none() {
            self.stats.first_audio_pts_us = Some(pts_us);
        }
        Ok(())
    }

    /// Close the current segment cleanly (`Muxer::finish()`), count it, and
    /// mark the recorder finished. Idempotent-ish: a second call returns
    /// [`RecorderError::Finished`]. Also called from `Drop` (best effort,
    /// errors ignored there) so a panicked/abandoned take still lands as a
    /// playable file.
    pub fn finish(&mut self) -> Result<(), RecorderError> {
        if self.stats.finished {
            return Err(RecorderError::Finished);
        }
        if self.stats.stopped {
            // A terminal error already closed (or abandoned) the segment;
            // don't pretend a clean finish happened.
            self.stats.finished = true;
            return Err(RecorderError::Stopped(
                "recorder stopped before finish".into(),
            ));
        }
        self.close_current_segment();
        self.stats.finished = true;
        Ok(())
    }

    /// Cumulative statistics (accounting oracle for tests and logs).
    pub fn stats(&self) -> &RecorderStats {
        &self.stats
    }

    /// Name of the current/last segment (`REC_0001.avi`, …).
    pub fn current_segment_name(&self) -> &str {
        &self.segment_name
    }

    /// Index of the current/last segment.
    pub fn current_segment_index(&self) -> u32 {
        self.stats.current_segment_index
    }

    /// True once the recorder stopped on a terminal error.
    pub fn is_stopped(&self) -> bool {
        self.stats.stopped
    }

    // -- internals ----------------------------------------------------------

    fn ensure_writable(&self) -> Result<(), RecorderError> {
        if self.stats.finished {
            return Err(RecorderError::Finished);
        }
        if self.stats.stopped {
            return Err(RecorderError::Stopped("recorder is stopped".into()));
        }
        Ok(())
    }

    /// Rollover triggers 1 (size) and 2 (wall-clock), checked before every
    /// write. Trigger 3 (storage error) lives in
    /// `write_packet_with_rolls`.
    fn maybe_roll_for_bounds(&mut self, pts_us: u64) -> Result<(), RecorderError> {
        if let Some(base) = self.segment_base_us {
            if pts_us.saturating_sub(base) >= self.config.segment_wallclock_us {
                self.roll(RollReason::WallClock, pts_us)?;
            }
        }
        if let Some(muxer) = &self.muxer {
            if muxer.bytes_written() >= self.config.segment_size_bytes {
                self.roll(RollReason::Size, pts_us)?;
            }
        }
        Ok(())
    }

    /// Write one packet, rolling on [`MuxerError::SegmentFull`] (the 2 GiB
    /// ceiling — the packet was *not* written, so a retry on the fresh
    /// segment is exact) and, per policy, on other write errors. Returns
    /// the video `pts_frame` actually written (audio returns 0 — the
    /// caller tracks its own sample counter).
    ///
    /// The PTS is re-derived *at write time* from the live segment state
    /// (video slot from `segment_base_us`, audio from
    /// `audio_samples_in_segment`), so a packet that rolls to a fresh
    /// segment mid-write automatically restarts that segment's timeline
    /// at 0 instead of inheriting the abandoned segment's coordinates.
    fn write_packet_with_rolls(
        &mut self,
        pts_us: u64,
        packet: PacketOut<'_>,
    ) -> Result<u64, RecorderError> {
        // Attempt 0: current segment. Attempt 1: the freshly rolled one.
        for attempt in 0..=1 {
            let index = self.stats.current_segment_index;
            let result = {
                let muxer = self.muxer.as_mut().expect("segment open");
                match &packet {
                    PacketOut::Video { jpeg } => {
                        let base = self.segment_base_us.expect("video write: base set");
                        let slot = Self::video_slot(pts_us, base, self.config.video.frame_rate);
                        muxer.as_mut().write_video(slot, jpeg).map(|()| slot)
                    }
                    PacketOut::Audio { pcm } => {
                        let pts_sample = self.audio_samples_in_segment;
                        muxer
                            .as_mut()
                            .write_audio(pts_sample, pcm)
                            .map(|()| pts_sample)
                    }
                }
            };
            match result {
                Ok(written_pts) => return Ok(written_pts),
                Err(MuxerError::SegmentFull { .. }) => {
                    // Pre-checked ceiling: no bytes for this packet landed.
                    // Roll and retry once on the next segment.
                    println!(
                        "recorder: segment {} hit the Avi10 ceiling — rolling to the next file",
                        segment_file_name(index)
                    );
                    if attempt == 0 {
                        self.roll(RollReason::SegmentFull, pts_us)?;
                    }
                }
                Err(e) => {
                    println!(
                        "WARNING: recorder: write error on segment {} (attempt {attempt}): {e}",
                        segment_file_name(index)
                    );
                    match self.config.error_policy {
                        ErrorPolicy::Stop => {
                            self.close_current_segment();
                            self.stats.stopped = true;
                            return Err(RecorderError::Write {
                                index,
                                source: Box::new(e),
                            });
                        }
                        ErrorPolicy::Roll => {
                            if attempt == 0 {
                                self.stats.rolls_error += 1;
                                self.close_current_segment();
                                if let Err(open_err) = self.open_next_segment(Some(pts_us)) {
                                    self.stats.stopped = true;
                                    return Err(open_err);
                                }
                            }
                        }
                    }
                }
            }
        }
        // Both attempts burned (e.g. SegmentFull twice, or a write error on
        // the rolled segment too). Stop loudly — never loop, never pad.
        self.stats.stopped = true;
        Err(RecorderError::Stopped(
            "write failed on two consecutive segments".into(),
        ))
    }

    /// Trigger-agnostic roll: close the current segment cleanly, count the
    /// reason, open the next.
    fn roll(&mut self, reason: RollReason, at_pts_us: u64) -> Result<(), RecorderError> {
        self.close_current_segment();
        match reason {
            RollReason::Size => self.stats.rolls_size += 1,
            RollReason::WallClock => self.stats.rolls_wallclock += 1,
            RollReason::SegmentFull => self.stats.rolls_segment_full += 1,
            RollReason::WriteError => self.stats.rolls_error += 1,
        }
        self.open_next_segment(Some(at_pts_us))
    }

    /// `Muxer::finish()` the current segment (counting a failed trailer as
    /// an explicit error, never silent) and drop the muxer so the sink
    /// file closes.
    fn close_current_segment(&mut self) {
        if let Some(mut muxer) = self.muxer.take() {
            self.stats.last_segment_bytes = muxer.bytes_written();
            match muxer.finish() {
                Ok(()) => {
                    println!(
                        "recorder: segment {} closed cleanly ({} bytes)",
                        self.segment_name, self.stats.last_segment_bytes
                    );
                    self.stats.segments_completed += 1;
                }
                Err(e) => {
                    println!(
                        "ERROR: recorder: segment {} finish failed: {e} (file may be truncated)",
                        self.segment_name
                    );
                    self.stats.segment_finish_errors += 1;
                }
            }
        }
    }

    /// Find the next free `REC_%04d.avi` name (skipping existing files —
    /// never overwrite), create the file via the factory, open the muxer.
    fn open_next_segment(&mut self, at_pts_us: Option<u64>) -> Result<(), RecorderError> {
        let mut index = self.next_index;
        let mut probes = 0u32;
        loop {
            if probes >= MAX_SEGMENT_INDEX_PROBES {
                self.stats.stopped = true;
                return Err(RecorderError::IndexExhausted { from: index });
            }
            probes += 1;
            let name = segment_file_name(index);
            match self.factory.exists(&name) {
                Ok(true) => {
                    // Existing take — skip, never clobber (IS-5 hygiene).
                    index += 1;
                    continue;
                }
                Ok(false) => {}
                Err(e) => {
                    self.stats.stopped = true;
                    return Err(RecorderError::SegmentOpen { index, source: e });
                }
            }
            match self.factory.create(&name) {
                Ok(sink) => {
                    let muxer = match self.config.muxer_kind {
                        MuxerKind::Avi => Box::new(
                            AviMuxer::new_with_limit(
                                self.config.video,
                                self.config.audio,
                                sink,
                                self.config.avi10_limit_bytes,
                            )
                            .map_err(|e| RecorderError::Muxer(e))?,
                        ) as Box<dyn Muxer>,
                        MuxerKind::Mock => {
                            Box::new(MockMuxer::new_mock(
                                self.config.video,
                                self.config.audio,
                                sink,
                            )) as Box<dyn Muxer>
                        }
                    };
                    self.muxer = Some(muxer);
                    self.segment_base_us = at_pts_us;
                    self.last_video_slot = None;
                    self.audio_samples_in_segment = 0;
                    self.next_index = index + 1;
                    self.segment_name = name.clone();
                    self.stats.current_segment_index = index;
                    self.stats.segments_opened += 1;
                    println!(
                        "recorder: segment {} opened (muxer={:?})",
                        name, self.config.muxer_kind
                    );
                    return Ok(());
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    // Lost a race after `exists` said free — skip.
                    index += 1;
                    continue;
                }
                Err(e) => {
                    self.stats.stopped = true;
                    return Err(RecorderError::SegmentOpen { index, source: e });
                }
            }
        }
    }
}

impl Drop for Recorder {
    /// Best-effort clean close so an abandoned take still lands playable.
    /// Errors are ignored (a `Drop` cannot surface them) but
    /// `close_current_segment` already logged any `finish` failure.
    fn drop(&mut self) {
        if !self.stats.finished && !self.stats.stopped {
            self.close_current_segment();
            self.stats.finished = true;
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests — recorder *logic* (naming, triggers, drop accounting, policy).
// Pure std + MockMuxer where possible; the Avi/ffprobe proofs run in the
// harness bin. These compile on the device too (no host-only APIs).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Seek, SeekFrom, Write};
    use std::sync::{Arc, Mutex};

    fn qvga_10fps() -> VideoTrack {
        VideoTrack::new(320, 240, 10).unwrap()
    }

    fn mono16k() -> AudioTrack {
        AudioTrack::new(16_000, 1).unwrap()
    }

    fn mock_config() -> RecorderConfig {
        let mut cfg = RecorderConfig::new(qvga_10fps(), mono16k());
        cfg.muxer_kind = MuxerKind::Mock;
        cfg
    }

    /// In-memory factory: tracks created/existing names, returns `Cursor`
    /// sinks (MockMuxer discards them anyway). Optionally fails `create`
    /// after N successes to test the open-failure policy path.
    #[derive(Clone, Default)]
    struct MemFactory {
        existing: Arc<Mutex<Vec<String>>>,
        created: Arc<Mutex<Vec<String>>>,
        fail_create_after: Arc<Mutex<Option<u32>>>,
    }

    impl MemFactory {
        fn new() -> Self {
            Self::default()
        }

        fn with_existing(name: &str) -> Self {
            let f = Self::new();
            f.existing.lock().unwrap().push(name.to_string());
            f
        }

        fn created_names(&self) -> Vec<String> {
            self.created.lock().unwrap().clone()
        }
    }

    impl SegmentFactory for MemFactory {
        fn exists(&self, name: &str) -> io::Result<bool> {
            Ok(self.existing.lock().unwrap().iter().any(|n| n == name))
        }

        fn create(&self, name: &str) -> io::Result<Box<dyn WriteSeek>> {
            let fail = self.fail_create_after.lock().unwrap();
            if let Some(allowed) = *fail {
                let made = self.created.lock().unwrap().len() as u32;
                if made >= allowed {
                    return Err(io::Error::new(io::ErrorKind::StorageFull, "no space"));
                }
            }
            self.created.lock().unwrap().push(name.to_string());
            self.existing.lock().unwrap().push(name.to_string());
            Ok(Box::new(Cursor::new(Vec::new())))
        }
    }

    /// Shared in-memory file (same pattern as muxer.rs tests): keeps bytes
    /// alive after the muxer's `Box<dyn WriteSeek>` is dropped.
    #[derive(Clone)]
    struct SharedFile {
        buf: Arc<Mutex<Vec<u8>>>,
        pos: u64,
        /// After `fail_after` bytes are written, every write fails —
        /// simulates a card pulled mid-take.
        fail_after: Option<u64>,
    }

    impl SharedFile {
        fn new() -> Self {
            Self { buf: Arc::new(Mutex::new(Vec::new())), pos: 0, fail_after: None }
        }

        fn failing_after(n: u64) -> Self {
            Self { fail_after: Some(n), ..Self::new() }
        }
    }

    impl Write for SharedFile {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            if let Some(limit) = self.fail_after {
                if self.pos + data.len() as u64 > limit {
                    return Err(io::Error::new(io::ErrorKind::Other, "simulated card failure"));
                }
            }
            let mut buf = self.buf.lock().unwrap();
            let end = self.pos as usize + data.len();
            if end > buf.len() {
                buf.resize(end, 0);
            }
            buf[self.pos as usize..end].copy_from_slice(data);
            self.pos = end as u64;
            Ok(data.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Seek for SharedFile {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            let len = self.buf.lock().unwrap().len() as i64;
            let new = match pos {
                SeekFrom::Start(n) => n as i64,
                SeekFrom::End(n) => len + n,
                SeekFrom::Current(n) => self.pos as i64 + n,
            };
            if new < 0 {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before start"));
            }
            self.pos = new as u64;
            Ok(self.pos)
        }
    }

    /// Factory handing out SharedFiles, optionally failing after N bytes.
    #[derive(Clone)]
    struct SharedFileFactory {
        fail_after: Option<u64>,
        created: Arc<Mutex<Vec<String>>>,
    }

    impl SharedFileFactory {
        fn new() -> Self {
            Self { fail_after: None, created: Arc::new(Mutex::new(Vec::new())) }
        }

        fn failing_after(n: u64) -> Self {
            Self { fail_after: Some(n), created: Arc::new(Mutex::new(Vec::new())) }
        }
    }

    impl SegmentFactory for SharedFileFactory {
        fn exists(&self, _name: &str) -> io::Result<bool> {
            Ok(false)
        }
        fn create(&self, name: &str) -> io::Result<Box<dyn WriteSeek>> {
            self.created.lock().unwrap().push(name.to_string());
            Ok(Box::new(match self.fail_after {
                Some(n) => SharedFile::failing_after(n),
                None => SharedFile::new(),
            }))
        }
    }

    fn fake_jpeg(len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len + 4);
        out.extend_from_slice(&[0xFF, 0xD8]);
        out.extend((0..len).map(|i| (i as u8).wrapping_mul(37)));
        out.extend_from_slice(&[0xFF, 0xD9]);
        out
    }

    #[test]
    fn segment_names_are_monotonic_zero_padded() {
        assert_eq!(segment_file_name(1), "REC_0001.avi");
        assert_eq!(segment_file_name(9), "REC_0009.avi");
        assert_eq!(segment_file_name(42), "REC_0042.avi");
        assert_eq!(segment_file_name(9999), "REC_9999.avi");
        // Beyond the pad width the name grows but stays monotonic.
        assert_eq!(segment_file_name(10_000), "REC_10000.avi");
    }

    #[test]
    fn existing_names_are_skipped_never_overwritten() {
        // Simulate a previous take already on the card.
        let factory = MemFactory::with_existing("REC_0001.avi");
        let rec = Recorder::new(mock_config(), factory.clone()).unwrap();
        // First segment must be REC_0002 — the pre-created file is skipped.
        assert_eq!(rec.current_segment_name(), "REC_0002.avi");
        assert_eq!(rec.current_segment_index(), 2);
        // Pre-created set only ever gained the new name, never replaced.
        let created = factory.created_names();
        assert_eq!(created, vec!["REC_0002.avi".to_string()]);
        // And the recorder keeps counting up, not reusing.
        drop(rec);
    }

    #[test]
    fn start_index_is_configurable() {
        let mut cfg = mock_config();
        cfg.first_segment_index = 7;
        let rec = Recorder::new(cfg, MemFactory::new()).unwrap();
        assert_eq!(rec.current_segment_name(), "REC_0007.avi");
    }

    #[test]
    fn video_slot_is_clock_derived_cfr() {
        // 10 fps → slot = µs / 100_000.
        assert_eq!(Recorder::video_slot(0, 0, 10), 0);
        assert_eq!(Recorder::video_slot(99_999, 0, 10), 0);
        assert_eq!(Recorder::video_slot(100_000, 0, 10), 1);
        assert_eq!(Recorder::video_slot(1_000_000, 0, 10), 10);
        // Segment-relative: base subtracted first.
        assert_eq!(Recorder::video_slot(5_000_000, 4_000_000, 10), 10);
        // A non-zero base below pts never underflows (saturating).
        assert_eq!(Recorder::video_slot(10, 1_000_000, 10), 0);
    }

    #[test]
    fn healthy_take_writes_all_frames_and_samples() {
        let factory = MemFactory::new();
        let mut rec = Recorder::new(mock_config(), factory).unwrap();
        // 2 s at 10 fps + 100 audio chunks of 320 samples (20 ms each).
        let jpeg = fake_jpeg(64);
        let mut written = 0;
        for i in 0..100u64 {
            let t = i * 20_000; // 20 ms ticks on the shared clock
            if i % 5 == 0 {
                let out = rec.write_video_frame(t, &jpeg).unwrap();
                assert_eq!(out, VideoOutcome::Written { pts_frame: i / 5 });
                written += 1;
            }
            let samples = [0i16; 320];
            rec.write_audio_samples(t, &samples).unwrap();
        }
        let s = rec.stats();
        assert_eq!(s.video_written, written);
        assert_eq!(s.video_written, 20);
        assert_eq!(s.video_dropped, 0);
        assert_eq!(s.audio_samples_written, 100 * 320);
        // CFR invariant: frames × period == samples / rate (exact here).
        assert_eq!(s.video_written * 100_000, s.audio_samples_written * 1_000_000 / 16_000);
        // Start delta: first video at t=0, first audio at t=0.
        assert_eq!(s.av_start_delta_us(), Some(0));
        rec.finish().unwrap();
        assert!(rec.stats().finished);
        assert_eq!(rec.stats().segments_completed, 1);
    }

    #[test]
    fn late_frames_are_dropped_never_duplicated() {
        let mut rec = Recorder::new(mock_config(), MemFactory::new()).unwrap();
        let jpeg = fake_jpeg(32);
        // Camera healthy for 500 ms (5 frames), then stalls, then the
        // stale backlog flushes late: every frame's slot is already
        // covered → all dropped, none duplicated, none re-timed.
        for i in 0..5u64 {
            let out = rec.write_video_frame(i * 100_000, &jpeg).unwrap();
            assert_eq!(out, VideoOutcome::Written { pts_frame: i });
        }
        for i in 0..5u64 {
            let out = rec.write_video_frame(i * 100_000 + 1_000, &jpeg).unwrap();
            assert_eq!(out, VideoOutcome::Dropped { slot: i });
        }
        // A burst *ahead* of the clock is written at its clock slots
        // (gaps in pts are honest; the muxer accepts non-contiguous pts).
        let out = rec.write_video_frame(2_000_000, &jpeg).unwrap();
        assert_eq!(out, VideoOutcome::Written { pts_frame: 20 });
        let s = rec.stats();
        assert_eq!(s.video_offered, 11);
        assert_eq!(s.video_written, 6);
        assert_eq!(s.video_dropped, 5);
        // Monotonic accounting identity — the drop proof.
        assert_eq!(s.video_written + s.video_dropped, s.video_offered);
    }

    #[test]
    fn wallclock_bound_rolls_to_a_new_file() {
        let mut cfg = mock_config();
        cfg.segment_wallclock_us = 1_000_000; // 1 s for the test
        let factory = MemFactory::new();
        let mut rec = Recorder::new(cfg, factory.clone()).unwrap();
        let jpeg = fake_jpeg(16);
        rec.write_video_frame(0, &jpeg).unwrap();
        rec.write_audio_samples(0, &[0i16; 320]).unwrap();
        // Still inside the bound — same segment.
        rec.write_video_frame(500_000, &jpeg).unwrap();
        assert_eq!(rec.current_segment_name(), "REC_0001.avi");
        // Crossing the bound rolls BEFORE this write lands.
        rec.write_video_frame(1_500_000, &jpeg).unwrap();
        assert_eq!(rec.current_segment_name(), "REC_0002.avi");
        assert_eq!(rec.stats().rolls_wallclock, 1);
        // New segment's base is the triggering pts → slot 0.
        assert_eq!(
            rec.write_video_frame(1_500_000 + 100_000, &jpeg).unwrap(),
            VideoOutcome::Written { pts_frame: 1 }
        );
        assert_eq!(factory.created_names(), vec!["REC_0001.avi", "REC_0002.avi"]);
        rec.finish().unwrap();
        assert_eq!(rec.stats().segments_completed, 2);
    }

    #[test]
    fn size_bound_rolls_to_a_new_file() {
        let mut cfg = mock_config();
        cfg.segment_size_bytes = 4_096; // tiny on purpose
        let mut rec = Recorder::new(cfg, MemFactory::new()).unwrap();
        let jpeg = fake_jpeg(1_024);
        // 1 KiB payloads → chunk accounting (8 B header + payload) crosses
        // 4 KiB after a handful of frames.
        let mut rolls_before = 0;
        for i in 0..64u64 {
            let _ = rec.write_video_frame(i * 100_000, &jpeg).unwrap();
            assert!(rec.stats().rolls_size >= rolls_before);
            rolls_before = rec.stats().rolls_size;
        }
        assert!(rec.stats().rolls_size >= 1, "size bound must have rolled");
        assert!(rec.stats().segments_opened >= 2);
        rec.finish().unwrap();
    }

    #[test]
    fn segment_full_from_avi10_ceiling_rolls_and_retries() {
        // Real AviMuxer with a tiny ceiling: the typed SegmentFull error
        // must roll to the next file and the packet must land there
        // (SegmentFull is pre-checked — no bytes were written).
        let mut cfg = RecorderConfig::new(qvga_10fps(), mono16k());
        cfg.avi10_limit_bytes = 8 * 1024; // header + a few packets
        let factory = SharedFileFactory::new();
        let mut rec = Recorder::new(cfg, factory.clone()).unwrap();
        let jpeg = fake_jpeg(2_048);
        let mut written = 0;
        let mut last_slot: Option<u64> = None;
        let mut last_segment = rec.current_segment_index();
        for i in 0..64u64 {
            let out = rec.write_video_frame(i * 100_000, &jpeg).unwrap();
            // After each SegmentFull roll the slot restarts at 0 on the
            // fresh segment — assert strict monotonicity *within* a
            // segment; reset the expectation when the segment changes.
            match out {
                VideoOutcome::Written { pts_frame } => {
                    let seg = rec.current_segment_index();
                    if seg != last_segment {
                        last_slot = None;
                        last_segment = seg;
                    }
                    if let Some(prev) = last_slot {
                        assert!(pts_frame > prev, "slot must advance within a segment");
                    }
                    last_slot = Some(pts_frame);
                    written += 1;
                }
                VideoOutcome::Dropped { slot } => panic!("unexpected drop at slot {slot}"),
            }
        }
        assert!(rec.stats().rolls_segment_full >= 1);
        assert_eq!(rec.stats().video_written, written);
        assert_eq!(rec.stats().video_dropped, 0);
        let names = factory.created.lock().unwrap().clone();
        assert!(names.len() >= 2, "expected multiple segments, got {names:?}");
        rec.finish().unwrap();
        assert_eq!(
            rec.stats().segment_finish_errors, 0,
            "every segment closed cleanly"
        );
    }

    #[test]
    fn write_error_with_roll_policy_rolls_then_retries_once() {
        // Sink accepts the header (a few hundred bytes) then fails: the
        // write error triggers roll (policy Roll), the next segment
        // accepts the retried packet.
        let mut cfg = RecorderConfig::new(qvga_10fps(), mono16k());
        cfg.muxer_kind = MuxerKind::Avi;
        let factory = SharedFileFactory::failing_after(2_000);
        let mut rec = Recorder::new(cfg, factory.clone()).unwrap();
        let jpeg = fake_jpeg(1_500);
        for i in 0..8u64 {
            match rec.write_video_frame(i * 100_000, &jpeg) {
                Ok(_) => {}
                Err(RecorderError::Stopped(_)) => break,
                Err(RecorderError::Write { .. }) => break,
                Err(e) => panic!("unexpected error kind: {e}"),
            }
            if rec.stats().rolls_error > 0 {
                break;
            }
        }
        // Whether the first write or the retry exhausted the failing sink,
        // the recorder must have either rolled at least once or stopped
        // with a typed error — never panic, never loop.
        assert!(rec.stats().rolls_error >= 1 || rec.is_stopped());
        let names = factory.created.lock().unwrap().clone();
        assert!(names.len() >= 1);
    }

    #[test]
    fn stop_policy_terminates_on_first_write_error() {
        let mut cfg = RecorderConfig::new(qvga_10fps(), mono16k());
        cfg.muxer_kind = MuxerKind::Avi;
        cfg.error_policy = ErrorPolicy::Stop;
        let jpeg = fake_jpeg(2_048);
        // 1500 B: the AVI header (~300 B) lands, the first 1.5 KiB frame
        // does not — the very first write fails.
        let factory = SharedFileFactory::failing_after(1_500);
        let mut rec = Recorder::new(cfg, factory).unwrap();
        let mut saw_typed_error = false;
        for i in 0..8u64 {
            match rec.write_video_frame(i * 100_000, &jpeg) {
                Ok(_) => {}
                Err(RecorderError::Write { .. }) | Err(RecorderError::Stopped(_)) => {
                    saw_typed_error = true;
                    break;
                }
                Err(e) => panic!("unexpected error kind: {e}"),
            }
        }
        assert!(saw_typed_error, "failing sink must surface a typed error");
        assert!(rec.is_stopped());
        // Subsequent writes are refused, typed.
        assert!(matches!(
            rec.write_video_frame(9_000_000, &jpeg),
            Err(RecorderError::Stopped(_))
        ));
        // finish() after a stop reports Stopped rather than a fake clean
        // close.
        assert!(matches!(rec.finish(), Err(RecorderError::Stopped(_))));
    }

    #[test]
    fn open_failure_stops_with_typed_diagnostic() {
        // Full volume: even the first create fails.
        let factory = MemFactory::new();
        *factory.fail_create_after.lock().unwrap() = Some(0);
        let err = expect_err(Recorder::new(mock_config(), factory));
        assert!(matches!(err, RecorderError::SegmentOpen { index: 1, .. }));
    }

    #[test]
    fn mid_take_open_failure_after_roll_stops() {
        // First segment opens; the second cannot (volume filled mid-take).
        let mut cfg = mock_config();
        cfg.segment_wallclock_us = 1_000_000;
        let factory = MemFactory::new();
        *factory.fail_create_after.lock().unwrap() = Some(1);
        let mut rec = Recorder::new(cfg, factory).unwrap();
        let jpeg = fake_jpeg(32);
        rec.write_video_frame(0, &jpeg).unwrap();
        let err = rec
            .write_video_frame(2_000_000, &jpeg)
            .unwrap_err();
        assert!(matches!(err, RecorderError::SegmentOpen { index: 2, .. }));
        assert!(rec.is_stopped());
    }

    #[test]
    fn writes_after_finish_are_typed_errors() {
        let mut rec = Recorder::new(mock_config(), MemFactory::new()).unwrap();
        rec.write_video_frame(0, &fake_jpeg(8)).unwrap();
        rec.finish().unwrap();
        assert!(matches!(
            rec.write_video_frame(100_000, &fake_jpeg(8)),
            Err(RecorderError::Finished)
        ));
        assert!(matches!(
            rec.write_audio_samples(0, &[0i16; 320]),
            Err(RecorderError::Finished)
        ));
        assert!(matches!(rec.finish(), Err(RecorderError::Finished)));
    }

    /// `Result<Recorder, _>::unwrap_err` without the `T: Debug` bound
    /// (`Recorder` holds trait objects and cannot derive `Debug`).
    fn expect_err<T>(res: Result<T, RecorderError>) -> RecorderError {
        match res {
            Ok(_) => panic!("expected an error, got Ok"),
            Err(e) => e,
        }
    }

    #[test]
    fn config_validation_rejects_degenerate_inputs() {
        let bad = VideoTrack { width: 0, height: 240, frame_rate: 10 };
        let err = expect_err(Recorder::new(
            RecorderConfig::new(bad, mono16k()),
            MemFactory::new(),
        ));
        assert!(matches!(err, RecorderError::InvalidConfig(_)));

        let mut cfg = mock_config();
        cfg.segment_size_bytes = 0;
        assert!(matches!(
            expect_err(Recorder::new(cfg, MemFactory::new())),
            RecorderError::InvalidConfig(_)
        ));

        let mut cfg = mock_config();
        cfg.segment_wallclock_us = 0;
        assert!(matches!(
            expect_err(Recorder::new(cfg, MemFactory::new())),
            RecorderError::InvalidConfig(_)
        ));

        let mut cfg = mock_config();
        cfg.first_segment_index = 0;
        assert!(matches!(
            expect_err(Recorder::new(cfg, MemFactory::new())),
            RecorderError::InvalidConfig(_)
        ));
    }

    #[test]
    fn empty_audio_chunk_is_a_noop_and_unaligned_is_typed() {
        let mut rec = Recorder::new(mock_config(), MemFactory::new()).unwrap();
        rec.write_audio_samples(0, &[]).unwrap();
        assert_eq!(rec.stats().audio_chunks_written, 0);
        // Mono: any length is whole frames. Multi-channel needs alignment.
        let mut cfg = mock_config();
        cfg.audio = AudioTrack::new(16_000, 2).unwrap();
        let mut rec2 = Recorder::new(cfg, MemFactory::new()).unwrap();
        let err = rec2.write_audio_samples(0, &[0i16; 3]).unwrap_err();
        assert!(matches!(err, RecorderError::Write { .. }));
    }

    #[test]
    fn empty_video_frame_is_typed() {
        let mut rec = Recorder::new(mock_config(), MemFactory::new()).unwrap();
        let err = rec.write_video_frame(0, &[]).unwrap_err();
        assert!(matches!(err, RecorderError::Write { .. }));
    }

    #[test]
    fn drop_closes_the_segment_cleanly() {
        let factory = MemFactory::new();
        {
            let mut rec = Recorder::new(mock_config(), factory.clone()).unwrap();
            rec.write_video_frame(0, &fake_jpeg(8)).unwrap();
            // No finish() — abandonment must not truncate silently.
        }
        // MockMuxer::finish is infallible; the accounting is the proof.
        // (The file-level proof is the harness bin with real AviMuxer.)
        assert_eq!(factory.created_names(), vec!["REC_0001.avi".to_string()]);
    }
}
