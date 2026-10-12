//! AVI recording muxer — our own `Muxer` trait over `oxideav-avi` 0.0.10.
//!
//! # Plan contract (Todo 9, plan `esp32s3-av-stream-and-sd-recording.md`)
//!
//! * **`AviKind::Avi10`, NOT OpenDML.** Plan decision D-2 / IS-5: recordings
//!   must roll over into *multiple separate playable files*. `AviKind::OpenDml`
//!   rolls `RIFF AVIX` continuations *inside one file*, which a desktop player
//!   may or may not traverse — it does not satisfy IS-5. The rollover is ours:
//!   one `Avi10` file per segment, bounded far below the 2 GiB `Avi10` ceiling
//!   (Todo 10 wraps this trait with the 256 MiB segment bound). Using `Avi10`
//!   also removes the crate's documented/implementation OpenDML contradiction
//!   from our critical path — we never touch the OpenDML code.
//! * **Supply-chain exemption, cited (ADR-0001 Part B, `firmware/docs/adr-0001-packetization.md`).**
//!   `oxideav-avi` is plan-exempted from the "no 1-star crates" default *for a
//!   stated reason*, audited items 1/2/5/9 = CLEAN: no `build.rs`, pure Rust,
//!   all-permissive licenses, and no H.264 path anywhere in the tree. The
//!   exemption argument itself is test evidence: 97 integration-test binaries /
//!   629 `#[test]` functions (incl. black-box `avi_ffmpeg` round-trips and a
//!   byte-golden mux hash) plus cargo-fuzz targets. It is pinned `=0.0.10`
//!   exactly, and wrapped *here*, so an upstream break touches one adapter file.
//! * **`finish()` is idempotent and dropped-frame-safe.** A video packet with no
//!   matching audio (the tail of a session where the mic chunk lost the race)
//!   must not corrupt the file: `write_trailer` back-patches real counts, so
//!   `finish()` succeeds; a second `finish()` is a no-op `Ok(())`.
//! * **The 2 GiB `Avi10` limit surfaces as the typed error
//!   [`MuxerError::SegmentFull`]**, before the offending packet is written —
//!   never past it. Todo 10's recorder rolls to a new file on that variant.
//!
//! # Architectural rule (enables the host proof)
//!
//! This module is **pure `std` + `oxideav-avi`/`oxideav-core` only** — no
//! `esp_idf_*` import, ever. The ESP32-S3 specifics (SD card via VFS) enter
//! only at the *call site* (Todo 10's recorder), which hands us any
//! `Box<dyn WriteSeek>`: the VFS file on device, a `Cursor<Vec<u8>>` or a
//! `std::fs::File` in host tests. That is what lets `harness/` `#[path]`-include
//! this file and prove the AVI output with real `ffprobe` on the host.
//!
//! # Track configuration (matches FFmpeg's AVI/MJPEG + PCM layout)
//!
//! * Video: MJPEG, `MJPG` FourCC in `BITMAPINFOHEADER` (24-bpp advisory).
//! * Audio: 16 kHz mono `pcm_s16le`; the crate synthesises the 18-byte
//!   `WAVEFORMATEX` with `wFormatTag = 1` from the codec id (verified in
//!   `oxideav-avi` 0.0.10 `packaging::pcm_synth_format_tag`).
//! * Video `pts` unit = frames (`strh` scale/rate = 1/frame_rate);
//!   audio `pts` unit = samples (`strh` scale/rate = 1/sample_rate).

use std::fmt;

use oxideav_avi::muxer as avi;
use oxideav_avi::muxer::AviKind;
use oxideav_core::{
    CodecId, CodecParameters, CodecTag, Error as CoreError, Muxer as OxMuxer, Packet, Rational,
    SampleFormat, StreamInfo, TimeBase, WriteSeek,
};

/// Stream index of the video track inside the AVI file (always 0).
pub const VIDEO_STREAM_INDEX: u32 = 0;
/// Stream index of the audio track inside the AVI file (always 1).
pub const AUDIO_STREAM_INDEX: u32 = 1;

/// Default video cadence: 25 fps (`avih.dwMicroSecPerFrame` = 40 000).
pub const DEFAULT_FRAME_RATE: u32 = 25;
/// Recording audio: 16 kHz — plan-pinned; matches the RTSP µ-law track rate.
pub const AUDIO_SAMPLE_RATE: u32 = 16_000;
/// Recording audio: mono. PDM mic is a single channel.
pub const AUDIO_CHANNELS: u16 = 1;
/// Bytes per audio sample frame: `pcm_s16le` × 1 channel.
pub const BYTES_PER_SAMPLE_FRAME: usize = 2;

/// Hard `Avi10` ceiling enforced *by us, before* the write.
///
/// The crate refuses once the running file would cross
/// `2 GiB - 1 KiB` (`Error::Unsupported`, checked *after* emitting the chunk).
/// We stop 1 MiB earlier with a typed error **before** the packet touches the
/// sink, so a rejected packet never lands on disk and Todo 10 can roll the
/// segment on `SegmentFull` without unpicking a half-written chunk.
pub const AVI10_LIMIT_BYTES: u64 = 2 * 1024 * 1024 * 1024 - 1024 * 1024;

/// Video track geometry handed to [`Muxer::new`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoTrack {
    pub width: u32,
    pub height: u32,
    /// Nominal frames per second (AVI `strh.dwRate/dwScale` = rate/1).
    pub frame_rate: u32,
}

impl VideoTrack {
    /// Build a track descriptor; rejects degenerate geometry with a typed
    /// error instead of producing an unplayable header.
    pub fn new(width: u32, height: u32, frame_rate: u32) -> Result<Self, MuxerError> {
        if width == 0 || height == 0 {
            return Err(MuxerError::InvalidConfig(
                "video track width/height must be non-zero".into(),
            ));
        }
        if frame_rate == 0 {
            return Err(MuxerError::InvalidConfig(
                "video track frame_rate must be non-zero".into(),
            ));
        }
        Ok(Self {
            width,
            height,
            frame_rate,
        })
    }
}

/// Audio track format handed to [`Muxer::new`]. Samples are always
/// interleaved `pcm_s16le` (the OV3660 pipeline never produces float).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioTrack {
    pub sample_rate: u32,
    pub channels: u16,
}

impl AudioTrack {
    /// Build a track descriptor; rejects degenerate formats with a typed error.
    pub fn new(sample_rate: u32, channels: u16) -> Result<Self, MuxerError> {
        if sample_rate == 0 {
            return Err(MuxerError::InvalidConfig(
                "audio track sample_rate must be non-zero".into(),
            ));
        }
        if channels == 0 {
            return Err(MuxerError::InvalidConfig(
                "audio track channels must be non-zero".into(),
            ));
        }
        Ok(Self {
            sample_rate,
            channels,
        })
    }

    /// Bytes per interleaved sample frame (`channels` × 2 for s16le).
    pub fn bytes_per_frame(&self) -> usize {
        self.channels as usize * BYTES_PER_SAMPLE_FRAME
    }
}

/// Typed errors for the recording path. No `thiserror`: house style is
/// hand-written `Display` + `Error` so the firmware dependency graph stays
/// exactly what the plan's dependency table lists.
#[derive(Debug)]
pub enum MuxerError {
    /// The sink failed (SD card unplugged, volume full, VFS error).
    Io(std::io::Error),
    /// Any other `oxideav-avi` failure, kept as its rendered message.
    Backend(String),
    /// The `Avi10` 2 GiB ceiling would be crossed by the offered packet.
    /// **No bytes for that packet were written.** The recorder must
    /// `finish()` the current file and roll to the next segment — it must
    /// never retry the write on the same muxer.
    SegmentFull { limit_bytes: u64 },
    /// Track geometry/format is degenerate (zero width/rate/channels...).
    InvalidConfig(String),
    /// A packet violates its track's framing (e.g. PCM payload not a
    /// whole number of sample frames).
    InvalidPacket(String),
    /// `finish()` already completed; the muxer is closed for writes.
    Finished,
}

impl fmt::Display for MuxerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MuxerError::Io(e) => write!(f, "avi muxer: I/O error: {e}"),
            MuxerError::Backend(msg) => write!(f, "avi muxer backend: {msg}"),
            MuxerError::SegmentFull { limit_bytes } => write!(
                f,
                "avi muxer: segment full at {limit_bytes} bytes (Avi10 2 GiB ceiling); \
                 finish this file and roll to a new segment"
            ),
            MuxerError::InvalidConfig(msg) => write!(f, "avi muxer config: {msg}"),
            MuxerError::InvalidPacket(msg) => write!(f, "avi muxer packet: {msg}"),
            MuxerError::Finished => write!(f, "avi muxer: already finished"),
        }
    }
}

impl std::error::Error for MuxerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MuxerError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<CoreError> for MuxerError {
    fn from(e: CoreError) -> Self {
        match e {
            CoreError::Io(io) => MuxerError::Io(io),
            // The crate's own Avi10 overrun: same typed variant as our
            // pre-check so the recorder's roll logic is one match arm.
            CoreError::Unsupported(ref msg) if msg.contains("2 GiB") => {
                MuxerError::SegmentFull {
                    limit_bytes: AVI10_LIMIT_BYTES,
                }
            }
            other => MuxerError::Backend(other.to_string()),
        }
    }
}

/// Our container sink abstraction — implemented by [`AviMuxer`] (real AVI
/// file) and [`MockMuxer`] (in-RAM packet log for on-device/host tests).
pub trait Muxer {
    /// Open a muxer on `sink` with the given tracks. `AviMuxer` writes the
    /// AVI header immediately; `MockMuxer` discards `sink`.
    fn new(
        video: VideoTrack,
        audio: AudioTrack,
        sink: Box<dyn WriteSeek>,
    ) -> Result<Self, MuxerError>
    where
        Self: Sized;

    /// Append one JPEG-compressed video frame. `pts_frame` counts frames
    /// from the start of *this segment* (0-based); every AVI video packet
    /// is a keyframe (MJPEG intra-only).
    fn write_video(&mut self, pts_frame: u64, jpeg: &[u8]) -> Result<(), MuxerError>;

    /// Append one `pcm_s16le` payload. `pts_sample` counts sample frames
    /// from the start of *this segment* (0-based). The payload must be a
    /// whole number of sample frames (`channels` × 2-byte aligned).
    fn write_audio(&mut self, pts_sample: u64, pcm_s16le: &[u8]) -> Result<(), MuxerError>;

    /// Close the segment. Idempotent: the first call writes the trailer
    /// (safe even when the last video frame has no matching audio), any
    /// later call is a no-op `Ok(())`.
    fn finish(&mut self) -> Result<(), MuxerError>;

    /// Bytes committed to the sink so far (chunk headers included; exact
    /// for `AviMuxer`, simulated for `MockMuxer`).
    fn bytes_written(&self) -> u64;

    /// True once `finish()` has completed.
    fn is_finished(&self) -> bool;
}

/// Real AVI 1.0 (`AviKind::Avi10`) muxer over `oxideav-avi =0.0.10`.
///
/// Sink is `Box<dyn WriteSeek>`: seekable by contract — the SD card through
/// ESP-IDF VFS is, and so are host files and `Cursor`s. The crate
/// back-patches RIFF/movi/avih/idx1 sizes at trailer time, which is why the
/// sink must seek.
pub struct AviMuxer {
    inner: Box<dyn OxMuxer>,
    video: VideoTrack,
    audio: AudioTrack,
    limit_bytes: u64,
    bytes_out: u64,
    finished: bool,
}

impl AviMuxer {
    /// Open with an explicit Avi10 byte ceiling. Defaults to
    /// [`AVI10_LIMIT_BYTES`]; tests use a tiny limit to prove the typed
    /// `SegmentFull` rollover signal without writing 2 GiB.
    pub fn new_with_limit(
        video: VideoTrack,
        audio: AudioTrack,
        sink: Box<dyn WriteSeek>,
        limit_bytes: u64,
    ) -> Result<Self, MuxerError> {
        // Re-run the typed constructor checks: callers can build the structs
        // literally and bypass `VideoTrack::new` / `AudioTrack::new`.
        let video = VideoTrack::new(video.width, video.height, video.frame_rate)?;
        let audio = AudioTrack::new(audio.sample_rate, audio.channels)?;
        if limit_bytes < 4096 {
            return Err(MuxerError::InvalidConfig(
                "segment limit below 4 KiB cannot hold an AVI header".into(),
            ));
        }

        // Video: MJPEG. `params.tag = MJPG` is the crate's canonical path
        // for `BITMAPINFOHEADER.biCompression` (verified: packaging.rs
        // `video_fourcc` reads `params.tag` first).
        let mut vparams = CodecParameters::video(CodecId::new("mjpeg"))
            .with_tag(CodecTag::fourcc(b"MJPG"));
        vparams.width = Some(video.width);
        vparams.height = Some(video.height);
        vparams.frame_rate = Some(Rational::new(video.frame_rate as i64, 1));
        let vstream = StreamInfo {
            index: VIDEO_STREAM_INDEX,
            time_base: TimeBase::new(1, video.frame_rate as i64),
            duration: None,
            start_time: Some(0),
            params: vparams,
        };

        // Audio: 16 kHz mono pcm_s16le. The crate synthesises the 18-byte
        // WAVEFORMATEX with wFormatTag = 1 straight from the codec id
        // (`pcm_synth_format_tag`), so we only declare rate/channels/depth.
        let mut aparams = CodecParameters::audio(CodecId::new("pcm_s16le"));
        aparams.sample_rate = Some(audio.sample_rate);
        aparams.channels = Some(audio.channels);
        aparams.sample_format = Some(SampleFormat::S16);
        let astream = StreamInfo {
            index: AUDIO_STREAM_INDEX,
            time_base: TimeBase::new(1, audio.sample_rate as i64),
            duration: None,
            start_time: Some(0),
            params: aparams,
        };

        // Avi10 — never OpenDML (plan D-2; see module docs).
        let mut inner = avi::open_with_kind(sink, &[vstream, astream], AviKind::Avi10)?;
        inner.write_header()?;

        Ok(Self {
            inner,
            video,
            audio,
            limit_bytes,
            bytes_out: 0,
            finished: false,
        })
    }

    fn ensure_open(&self) -> Result<(), MuxerError> {
        if self.finished {
            return Err(MuxerError::Finished);
        }
        Ok(())
    }

    /// Pre-flight the Avi10 ceiling: charge `payload + 8-byte chunk header +
    /// RIFF word pad` and reject *before* anything is written.
    fn charge(&mut self, payload: usize) -> Result<(), MuxerError> {
        let chunk = 8u64
            .checked_add(payload as u64)
            .and_then(|n| n.checked_add((payload & 1) as u64))
            .ok_or_else(|| {
                MuxerError::InvalidPacket("packet size overflows u64".into())
            })?;
        let projected = self.bytes_out.saturating_add(chunk);
        if projected > self.limit_bytes {
            return Err(MuxerError::SegmentFull {
                limit_bytes: self.limit_bytes,
            });
        }
        self.bytes_out = projected;
        Ok(())
    }
}

impl Muxer for AviMuxer {
    fn new(
        video: VideoTrack,
        audio: AudioTrack,
        sink: Box<dyn WriteSeek>,
    ) -> Result<Self, MuxerError> {
        Self::new_with_limit(video, audio, sink, AVI10_LIMIT_BYTES)
    }

    fn write_video(&mut self, pts_frame: u64, jpeg: &[u8]) -> Result<(), MuxerError> {
        self.ensure_open()?;
        self.charge(jpeg.len())?;
        let mut pkt = Packet::new(
            VIDEO_STREAM_INDEX,
            TimeBase::new(1, self.video.frame_rate as i64),
            jpeg.to_vec(),
        );
        pkt.pts = Some(pts_frame as i64);
        pkt.duration = Some(1);
        pkt.flags.keyframe = true;
        self.inner.write_packet(&pkt)?;
        Ok(())
    }

    fn write_audio(&mut self, pts_sample: u64, pcm_s16le: &[u8]) -> Result<(), MuxerError> {
        self.ensure_open()?;
        let bpf = self.audio.bytes_per_frame();
        if bpf == 0 || pcm_s16le.len() % bpf != 0 {
            return Err(MuxerError::InvalidPacket(format!(
                "PCM payload of {} bytes is not a multiple of the {}-byte sample frame",
                pcm_s16le.len(),
                bpf
            )));
        }
        self.charge(pcm_s16le.len())?;
        let samples = (pcm_s16le.len() / bpf) as i64;
        let mut pkt = Packet::new(
            AUDIO_STREAM_INDEX,
            TimeBase::new(1, self.audio.sample_rate as i64),
            pcm_s16le.to_vec(),
        );
        pkt.pts = Some(pts_sample as i64);
        pkt.duration = Some(samples);
        pkt.flags.keyframe = true;
        self.inner.write_packet(&pkt)?;
        Ok(())
    }

    fn finish(&mut self) -> Result<(), MuxerError> {
        if self.finished {
            return Ok(());
        }
        // Safe with a dropped frame: `write_trailer` back-patches the real
        // per-stream counts (frames / PCM samples actually written), so a
        // trailing video packet with no audio partner still yields a valid
        // playable file. Idempotent at *our* layer; the crate's trailer is
        // also internally guarded (`trailer_written`), belt and braces.
        self.inner.write_trailer()?;
        self.finished = true;
        Ok(())
    }

    fn bytes_written(&self) -> u64 {
        self.bytes_out
    }

    fn is_finished(&self) -> bool {
        self.finished
    }
}

/// Which track a recorded packet belongs to (mock packet log).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
}

/// One packet as recorded by [`MockMuxer`], in write order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedPacket {
    pub kind: TrackKind,
    pub pts: u64,
    pub payload: Vec<u8>,
}

/// In-RAM [`Muxer`] for tests: no file, no codec, full accounting.
///
/// Records every packet in call order so a test can assert the exact
/// interleaving the recorder produced, and models the same Avi10 ceiling
/// arithmetic as [`AviMuxer`] (chunk header + payload + RIFF word pad).
pub struct MockMuxer {
    audio: AudioTrack,
    packets: Vec<RecordedPacket>,
    bytes_out: u64,
    finished: bool,
}

impl MockMuxer {
    /// Open a mock muxer. `sink` is dropped — the point is that the trait
    /// shape matches the real muxer exactly.
    pub fn new_mock(_video: VideoTrack, audio: AudioTrack, _sink: Box<dyn WriteSeek>) -> Self {
        Self {
            audio,
            packets: Vec::new(),
            bytes_out: 0,
            finished: false,
        }
    }

    /// The packet log in write order.
    pub fn packets(&self) -> &[RecordedPacket] {
        &self.packets
    }

    /// Number of video packets written.
    pub fn video_packets(&self) -> usize {
        self.packets
            .iter()
            .filter(|p| p.kind == TrackKind::Video)
            .count()
    }

    /// Number of audio packets written.
    pub fn audio_packets(&self) -> usize {
        self.packets
            .iter()
            .filter(|p| p.kind == TrackKind::Audio)
            .count()
    }

    /// Total PCM sample frames written (payload bytes / sample-frame size).
    pub fn audio_samples(&self) -> u64 {
        let bpf = self.audio.bytes_per_frame() as u64;
        if bpf == 0 {
            return 0;
        }
        self.packets
            .iter()
            .filter(|p| p.kind == TrackKind::Audio)
            .map(|p| p.payload.len() as u64)
            .sum::<u64>()
            / bpf
    }

    /// Video frames written beyond the audio coverage — a *dropped frame*
    /// leaves the tail video packet(s) with no matching audio packet. The
    /// accounting Todo 10's recorder asserts after `finish()`.
    pub fn unmatched_video_packets(&self) -> usize {
        self.video_packets().saturating_sub(self.audio_packets())
    }
}

impl Muxer for MockMuxer {
    fn new(
        video: VideoTrack,
        audio: AudioTrack,
        sink: Box<dyn WriteSeek>,
    ) -> Result<Self, MuxerError> {
        Ok(Self::new_mock(video, audio, sink))
    }

    fn write_video(&mut self, pts_frame: u64, jpeg: &[u8]) -> Result<(), MuxerError> {
        if self.finished {
            return Err(MuxerError::Finished);
        }
        let chunk = 8 + jpeg.len() + (jpeg.len() & 1);
        self.bytes_out += chunk as u64;
        self.packets.push(RecordedPacket {
            kind: TrackKind::Video,
            pts: pts_frame,
            payload: jpeg.to_vec(),
        });
        Ok(())
    }

    fn write_audio(&mut self, pts_sample: u64, pcm_s16le: &[u8]) -> Result<(), MuxerError> {
        if self.finished {
            return Err(MuxerError::Finished);
        }
        let bpf = self.audio.bytes_per_frame();
        if bpf == 0 || pcm_s16le.len() % bpf != 0 {
            return Err(MuxerError::InvalidPacket(format!(
                "PCM payload of {} bytes is not a multiple of the {}-byte sample frame",
                pcm_s16le.len(),
                bpf
            )));
        }
        let chunk = 8 + pcm_s16le.len() + (pcm_s16le.len() & 1);
        self.bytes_out += chunk as u64;
        self.packets.push(RecordedPacket {
            kind: TrackKind::Audio,
            pts: pts_sample,
            payload: pcm_s16le.to_vec(),
        });
        Ok(())
    }

    fn finish(&mut self) -> Result<(), MuxerError> {
        // Idempotent; a dropped trailing video frame needs no repair here —
        // `unmatched_video_packets()` is the accounting a test/recorder reads.
        self.finished = true;
        Ok(())
    }

    fn bytes_written(&self) -> u64 {
        self.bytes_out
    }

    fn is_finished(&self) -> bool {
        self.finished
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Seek, SeekFrom, Write};
    use std::sync::{Arc, Mutex};

    fn qvga() -> VideoTrack {
        VideoTrack::new(320, 240, DEFAULT_FRAME_RATE).unwrap()
    }

    fn mono16k() -> AudioTrack {
        AudioTrack::new(AUDIO_SAMPLE_RATE, AUDIO_CHANNELS).unwrap()
    }

    /// JPEG-shaped synthetic frame (`FF D8` ... `FF D9`) — the container
    /// treats it as opaque MJPG bytes; the real-fixture proof runs in the
    /// harness bin against `ffprobe`.
    fn fake_jpeg(seed: u8, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len + 4);
        out.extend_from_slice(&[0xFF, 0xD8]);
        out.extend((0..len).map(|i| seed.wrapping_add(i as u8).wrapping_mul(37)));
        out.extend_from_slice(&[0xFF, 0xD9]);
        out
    }

    fn sine_chunk(start_sample: u64, samples: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(samples * 2);
        for i in 0..samples {
            let t = (start_sample + i as u64) as f64 / AUDIO_SAMPLE_RATE as f64;
            let v = (0.3 * f64::sin(2.0 * std::f64::consts::PI * 440.0 * t) * i16::MAX as f64) as i16;
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    }

    const SAMPLES_PER_FRAME: usize = AUDIO_SAMPLE_RATE as usize / DEFAULT_FRAME_RATE as usize;

    /// Shared in-memory file: `Arc<Mutex<Vec<u8>>>` keeps the bytes alive
    /// after the muxer (which owns the `Box<dyn WriteSeek>`) is dropped.
    /// `pos` is this handle's write cursor, exactly like a real file.
    #[derive(Clone)]
    struct SharedFile {
        buf: Arc<Mutex<Vec<u8>>>,
        pos: u64,
    }

    impl SharedFile {
        fn new() -> Self {
            Self {
                buf: Arc::new(Mutex::new(Vec::new())),
                pos: 0,
            }
        }

        fn bytes(&self) -> Vec<u8> {
            self.buf.lock().unwrap().clone()
        }
    }

    impl Write for SharedFile {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            let mut buf = self.buf.lock().unwrap();
            let end = self.pos as usize + data.len();
            if end > buf.len() {
                buf.resize(end, 0);
            }
            buf[self.pos as usize..end].copy_from_slice(data);
            self.pos = end as u64;
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Seek for SharedFile {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            let len = self.buf.lock().unwrap().len() as i64;
            let new = match pos {
                SeekFrom::Start(n) => n as i64,
                SeekFrom::End(n) => len + n,
                SeekFrom::Current(n) => self.pos as i64 + n,
            };
            if new < 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "seek before start",
                ));
            }
            self.pos = new as u64;
            Ok(self.pos)
        }
    }

    /// Open the crate's demuxer over raw AVI bytes with an `MJPG` resolver
    /// registered (PCM needs none — the container synthesises it).
    fn demux(bytes: Vec<u8>) -> Box<dyn oxideav_core::Demuxer> {
        let mut reg = oxideav_core::CodecRegistry::new();
        reg.register(
            oxideav_core::CodecInfo::new(CodecId::new("mjpeg"))
                .tag(CodecTag::fourcc(b"MJPG")),
        );
        let rs: Box<dyn oxideav_core::ReadSeek> = Box::new(Cursor::new(bytes));
        oxideav_avi::demuxer::open(rs, &reg).expect("demux open")
    }

    #[test]
    fn mock_interleaved_session_accounting() {
        let mut m = MockMuxer::new_mock(qvga(), mono16k(), Box::new(Cursor::new(Vec::new())));
        for frame in 0..3u64 {
            m.write_video(frame, &fake_jpeg(frame as u8, 1024)).unwrap();
            let pcm = sine_chunk(frame * SAMPLES_PER_FRAME as u64, SAMPLES_PER_FRAME);
            m.write_audio(frame * SAMPLES_PER_FRAME as u64, &pcm).unwrap();
        }
        assert_eq!(m.video_packets(), 3);
        assert_eq!(m.audio_packets(), 3);
        assert_eq!(m.unmatched_video_packets(), 0);
        assert_eq!(m.audio_samples(), 3 * SAMPLES_PER_FRAME as u64);
        // Interleaving order: V A V A V A; video pts 0,1,2; audio pts 0,640,1280.
        let kinds: Vec<TrackKind> = m.packets().iter().map(|p| p.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TrackKind::Video,
                TrackKind::Audio,
                TrackKind::Video,
                TrackKind::Audio,
                TrackKind::Video,
                TrackKind::Audio,
            ]
        );
        let video_pts: Vec<u64> = m
            .packets()
            .iter()
            .filter(|p| p.kind == TrackKind::Video)
            .map(|p| p.pts)
            .collect();
        assert_eq!(video_pts, vec![0, 1, 2]);
        let audio_pts: Vec<u64> = m
            .packets()
            .iter()
            .filter(|p| p.kind == TrackKind::Audio)
            .map(|p| p.pts)
            .collect();
        assert_eq!(audio_pts, vec![0, 640, 1280]);
        // Byte accounting mirrors AVI chunk framing: 8-byte header + payload
        // (+1 RIFF pad for odd payloads; both payload kinds are even-sized here).
        let expected: u64 = m
            .packets()
            .iter()
            .map(|p| 8 + p.payload.len() as u64)
            .sum();
        assert_eq!(m.bytes_written(), expected);
        assert!(!m.is_finished());
        m.finish().unwrap();
        assert!(m.is_finished());
    }

    #[test]
    fn mock_finish_with_dropped_frame_is_safe() {
        let mut m = MockMuxer::new_mock(qvga(), mono16k(), Box::new(Cursor::new(Vec::new())));
        for frame in 0..3u64 {
            m.write_video(frame, &fake_jpeg(frame as u8, 512)).unwrap();
            let pcm = sine_chunk(frame * SAMPLES_PER_FRAME as u64, SAMPLES_PER_FRAME);
            m.write_audio(frame * SAMPLES_PER_FRAME as u64, &pcm).unwrap();
        }
        // The dropped frame: a 4th video packet whose audio chunk never came.
        m.write_video(3, &fake_jpeg(9, 512)).unwrap();

        m.finish().unwrap();
        assert!(m.is_finished());
        assert_eq!(m.video_packets(), 4);
        assert_eq!(m.audio_packets(), 3);
        assert_eq!(m.unmatched_video_packets(), 1);

        // finish() is idempotent — the recorder may call it from both the
        // normal path and a drop-guard.
        m.finish().unwrap();
        assert!(m.is_finished());

        // And the closed muxer rejects further writes with a typed error.
        assert!(matches!(
            m.write_video(4, &fake_jpeg(1, 64)),
            Err(MuxerError::Finished)
        ));
        assert!(matches!(
            m.write_audio(0, &[0u8; 2]),
            Err(MuxerError::Finished)
        ));
    }

    #[test]
    fn track_config_rejects_degenerate_geometry() {
        assert!(matches!(
            VideoTrack::new(0, 240, 25),
            Err(MuxerError::InvalidConfig(_))
        ));
        assert!(matches!(
            VideoTrack::new(320, 240, 0),
            Err(MuxerError::InvalidConfig(_))
        ));
        assert!(matches!(AudioTrack::new(0, 1), Err(MuxerError::InvalidConfig(_))));
        assert!(matches!(
            AudioTrack::new(16_000, 0),
            Err(MuxerError::InvalidConfig(_))
        ));
    }

    #[test]
    fn mock_rejects_unaligned_pcm_payload() {
        let mut m = MockMuxer::new_mock(qvga(), mono16k(), Box::new(Cursor::new(Vec::new())));
        assert!(matches!(
            m.write_audio(0, &[0u8; 3]),
            Err(MuxerError::InvalidPacket(_))
        ));
    }

    #[test]
    fn avi10_roundtrip_two_streams_and_dropped_frame() {
        let sink_file = SharedFile::new();
        let sink: Box<dyn WriteSeek> = Box::new(sink_file.clone());
        let mut m = AviMuxer::new(qvga(), mono16k(), sink).unwrap();

        // 4 paired frames, then a 5th video frame whose audio was dropped.
        for frame in 0..4u64 {
            m.write_video(frame, &fake_jpeg(frame as u8, 2048)).unwrap();
            let pcm = sine_chunk(frame * SAMPLES_PER_FRAME as u64, SAMPLES_PER_FRAME);
            m.write_audio(frame * SAMPLES_PER_FRAME as u64, &pcm).unwrap();
        }
        let last_frame = fake_jpeg(9, 2048);
        m.write_video(4, &last_frame).unwrap();

        // finish() must succeed despite the dropped frame, and be idempotent.
        m.finish().unwrap();
        m.finish().unwrap();
        assert!(m.is_finished());
        assert!(m.bytes_written() > 4 * 2048);
        // ...and reject further writes with the typed error.
        assert!(matches!(
            m.write_video(5, &fake_jpeg(1, 64)),
            Err(MuxerError::Finished)
        ));
        drop(m);

        // Re-open through the crate's own demuxer: exactly two streams,
        // mjpeg @ 320x240 and pcm_s16le @ 16 kHz mono.
        let mut dmx = demux(sink_file.bytes());
        let streams = dmx.streams();
        assert_eq!(streams.len(), 2, "AVI must declare exactly two streams");
        assert_eq!(streams[0].params.codec_id.as_str(), "mjpeg");
        assert_eq!(streams[0].params.width, Some(320));
        assert_eq!(streams[0].params.height, Some(240));
        assert_eq!(streams[1].params.codec_id.as_str(), "pcm_s16le");
        assert_eq!(streams[1].params.sample_rate, Some(AUDIO_SAMPLE_RATE));
        assert_eq!(streams[1].params.channels, Some(1));

        // All 9 packets come back: 5 video + 4 audio, last video intact.
        let mut video = 0usize;
        let mut audio = 0usize;
        let mut last_video: Option<Vec<u8>> = None;
        loop {
            match dmx.next_packet() {
                Ok(p) => {
                    if p.stream_index == VIDEO_STREAM_INDEX {
                        video += 1;
                        last_video = Some(p.data);
                    } else {
                        audio += 1;
                    }
                }
                Err(CoreError::Eof) => break,
                Err(e) => panic!("demux error: {e}"),
            }
        }
        assert_eq!(video, 5, "5 video frames incl. the dropped-audio tail");
        assert_eq!(audio, 4, "4 audio packets");
        assert_eq!(last_video.as_deref(), Some(last_frame.as_slice()));
    }

    #[test]
    fn avi10_segment_full_is_typed_and_prechecked() {
        let sink_file = SharedFile::new();
        let sink: Box<dyn WriteSeek> = Box::new(sink_file.clone());
        // Tiny ceiling: the header fits, a handful of packets don't.
        const LIMIT: u64 = 8192;
        let mut m = AviMuxer::new_with_limit(qvga(), mono16k(), sink, LIMIT).unwrap();
        let frame = fake_jpeg(1, 4096);
        let mut hits = 0;
        for i in 0..8 {
            match m.write_video(i, &frame) {
                Ok(()) => {}
                Err(MuxerError::SegmentFull { limit_bytes }) => {
                    assert_eq!(limit_bytes, LIMIT);
                    hits += 1;
                    break;
                }
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        assert_eq!(hits, 1, "expected exactly one typed SegmentFull");
        // The ceiling is a *pre-check*: committed bytes never exceed it.
        assert!(m.bytes_written() <= LIMIT);
        // The segment still closes cleanly (Todo 10 rolls on this).
        m.finish().unwrap();
        assert!(m.is_finished());
        drop(m);
        assert!(sink_file.bytes().len() as u64 <= LIMIT);
    }

    #[test]
    fn avi10_rejects_unaligned_pcm_before_write() {
        let sink: Box<dyn WriteSeek> = Box::new(Cursor::new(Vec::new()));
        let mut m = AviMuxer::new(qvga(), mono16k(), sink).unwrap();
        let before = m.bytes_written();
        assert!(matches!(
            m.write_audio(0, &[0u8; 5]),
            Err(MuxerError::InvalidPacket(_))
        ));
        assert_eq!(m.bytes_written(), before, "no bytes charged on reject");
    }

    #[test]
    fn avi10_rejects_degenerate_track_configs() {
        // Bypassing the constructors still fails typed at open time.
        let bad = VideoTrack {
            width: 320,
            height: 240,
            frame_rate: 0,
        };
        let sink: Box<dyn WriteSeek> = Box::new(Cursor::new(Vec::new()));
        assert!(AviMuxer::new_with_limit(bad, mono16k(), sink, 1 << 20).is_err());
        let bad_audio = AudioTrack {
            sample_rate: 0,
            channels: 1,
        };
        let sink: Box<dyn WriteSeek> = Box::new(Cursor::new(Vec::new()));
        assert!(AviMuxer::new_with_limit(qvga(), bad_audio, sink, 1 << 20).is_err());
        let sink: Box<dyn WriteSeek> = Box::new(Cursor::new(Vec::new()));
        assert!(AviMuxer::new_with_limit(qvga(), mono16k(), sink, 128).is_err());
    }
}
