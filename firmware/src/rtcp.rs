//! Periodic RTCP Sender Reports with the SNTP wallclock (plan Todo 13).
//!
//! Plan contract, quoted from `.omo/plans/esp32s3-av-stream-and-sd-recording.md`
//! Todo 13:
//!
//! > **References:** `ffprobe` derives `start_time` from the first RTCP
//! > Sender Report. Without SRs, `start_time` is `N/A` and the A/V-sync
//! > assertion is meaningless. RTP timestamps are 32-bit: at 90 kHz the wrap
//! > period is 2³²/90000 ≈ 47,721 s ≈ 13.3 hours — long but finite, so the
//! > wrap must be handled rather than ignored.
//! >
//! > **Do:** emit an RTCP Sender Report per track at least every 5 seconds and
//! > on session teardown, carrying the NTP wallclock **obtained from Todo 14's
//! > SNTP sync** (never a synthesised one), RTP timestamp, packet and octet
//! > counts. Handle incoming RTCP Receiver Reports. Make the 32-bit timestamp
//! > wrap explicit in the code rather than relying on natural overflow.
//! >
//! > **Acceptance:** `ffprobe -show_entries stream=start_time` returns a
//! > numeric value for both tracks; the delta is ≤ 250 ms. The reported NTP
//! > matches `sntp.last_sync_utc` to within one poll interval.
//! >
//! > **QA — failure:** unit-test the wrap at `0xFFFF_FFFF` → 0, and assert the
//! > reported time is continuous across the boundary. **With SNTP unavailable
//! > — no network, or the SNTP server unreachable — assert that Sender Reports
//! > are suppressed entirely and the log says "unsynced, SR suppressed",
//! > rather than being emitted with a fabricated wallclock.** That failure is
//! > the one that must not regress into a plausible-but-wrong value.
//!
//! # The cardinal rule: never fabricate a wallclock
//!
//! This is the plan's explicitly stated failure mode for this todo — "a quick
//! pass 'fixes' the N/A `start_time` by synthesising a timestamp and ships a
//! plausible-but-wrong clock." Therefore:
//!
//! - The NTP field of every Sender Report comes **only** from Todo 14's SNTP
//!   state: `net::may_send_rtcp()` (the suppression predicate) and
//!   `net::last_sync_utc()` (the wallclock anchor), read through
//!   [`SyncSample`] / [`SrClock`]. `net.rs` documents this in its own header:
//!   *"Todo 13 must read time via `is_synced` / `last_sync_utc`, never via a
//!   handle it owns."* This module owns no `EspSntp` handle.
//! - When SNTP has never synced, [`sr_action`] returns
//!   `SrAction::Suppress(SUPPRESSED_LOG)` — **zero bytes** are produced and
//!   the log line is exactly `unsynced, SR suppressed`. There is no branch
//!   anywhere in this module that invents a fallback time.
//! - When synced, the wallclock is the SNTP anchor (`last_sync_utc`) advanced
//!   by **monotonic** elapsed time ([`SrClock`]); re-anchoring on every resync
//!   keeps the reported NTP within one 60-minute poll interval of
//!   `last_sync_utc`, which is exactly the plan's acceptance bound. Monotonic
//!   time only ever *advances* the SNTP value — it never substitutes for it.
//!
//! # Layout: RFC 3550 §6.4.1, hand-rolled, zero new crates
//!
//! ```text
//!  0                   1                   2                   3
//!  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |V=2|P|    RC=0 |   PT=200      |             length            |  = 6
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                         SSRC of sender                        |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |              NTP timestamp, most significant word             |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |             NTP timestamp, least significant word             |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                         RTP timestamp                         |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                     sender's packet count                     |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                      sender's octet count                     |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! ```
//!
//! 28 bytes total; `length` is in 32-bit words **minus one** for the header
//! (7 words − 1 = 6). The plan locks "zero new crates" for this todo — the
//! whole packet is [`SenderReport::to_bytes`].
//!
//! # NTP time (64-bit, epoch 1900-01-01)
//!
//! [`system_time_to_ntp`] maps a `SystemTime` (UNIX epoch 1970) to NTP
//! seconds+fraction using [`NTP_UNIX_EPOCH_OFFSET_SECS`] = 2 208 988 800 s
//! (70 years incl. 17 leap days: 25 567 days × 86 400). Era check: the 64-bit
//! field's 32-bit seconds word is era 0 (valid 1900 … 2036-02-07; this
//! firmware's 2026 dates sit comfortably inside — the era wrap is documented,
//! not silently relied upon). Fraction = `nanos × 2³² / 10⁹`, floor.
//!
//! # Explicit 32-bit RTP-timestamp wrap (plan MUST DO)
//!
//! The SR's own `rtp_timestamp` field is the track counter mod 2³² (that is
//! what RFC 3550 carries); the **explicit** handling lives in the
//! time-computation/reporting path: [`rtp_delta_units`] /
//! [`rtp_delta_us`] define forward distance on the 2³² ring via
//! `wrapping_sub` (documented semantics, never plain `-`), and
//! [`RtpElapsed`] accumulates a u64 running total so reported elapsed time is
//! **continuous across `0xFFFF_FFFF → 0`** — asserted host-side in
//! `harness/tests/task13_rtcp.rs` (the plan QA-failure row) and by this
//! module's own tests.
//!
//! # Scheduling
//!
//! [`sr_due`] is a pure decision fn (≥ 5 s since the last SR, never early);
//! the first report of a session is always due (`last_sent = None`), which is
//! also how the teardown report is forced. [`spawn_sender_reports`] runs that
//! loop on a thread for the device: sinks are plain
//! `Fn(&[u8]) -> io::Result<()>` closures so wiring is
//! `move |p| sender.send_rtcp(p)` against Todo 11's
//! [`crate::rtsp::server::RtpSender`] (its doc header already reserves
//! `send_rtcp` for this todo), while the host tests inject capturing sinks —
//! the module references no `crate::` paths and compiles standalone via
//! `#[path]`.
//!
//! # Incoming Receiver Reports
//!
//! [`parse_rtcp`] walks a compound RTCP packet with bounds checks and returns
//! typed summaries ([`RtcpSummary`]); [`describe_rtcp`] renders the log line.
//! **Full RR semantics are deliberately not implemented**: loss/jitter/delay
//! fields are parsed far enough to never choke and are logged, not acted on
//! (documented here per the plan: "parse enough to not choke; forward/log").
//! Delivery wiring — routing `$`-interleaved odd channels from the connection
//! loop into this parser — belongs to the service owner (Todo 11 left
//! `serve_conn` dropping client `$` frames with a TODO; `rtsp/service.rs` is
//! outside this todo's lane), so the consumer call site lands with Todo 20
//! boot wiring.

use std::fmt;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Constants (plan-locked)
// ---------------------------------------------------------------------------

/// RTCP Sender Report packet type (RFC 3550 §6.4.1).
pub const PT_SR: u8 = 200;

/// RTCP Receiver Report packet type (RFC 3550 §6.5.1) — parsed/logged.
pub const PT_RR: u8 = 201;

/// SR interval: "at least every 5 seconds" (plan Todo 13 Do row).
pub const SR_INTERVAL_MS: u64 = 5_000;

/// Fixed SR size: 4 header + 4 SSRC + 8 NTP + 4 RTP ts + 4 pkts + 4 octets.
pub const SR_WIRE_LEN: usize = 28;

/// `length` field of the SR = total words (7) − 1 = 6 (RFC 3550: length is
/// in 32-bit words minus one for the header).
pub const SR_LENGTH_FIELD: u16 = 6;

/// Seconds between the NTP epoch (1900-01-01T00:00:00Z) and the UNIX epoch
/// (1970-01-01T00:00:00Z): 70 years = 25 567 days (17 of them leap) × 86 400
/// = 2 208 988 800. Every NTP mapping in this module goes through this one
/// documented constant.
pub const NTP_UNIX_EPOCH_OFFSET_SECS: u64 = 2_208_988_800;

/// The exact suppression log line the plan's QA-failure row greps for
/// (`"unsynced, SR suppressed"`). Emitted by [`spawn_sender_reports`];
/// carried by `SrAction::Suppress` so host tests can assert it verbatim.
pub const SUPPRESSED_LOG: &str = "unsynced, SR suppressed";

/// Video RTP clock (Hz) — `a=rtpmap:26 JPEG/90000` (plan lock).
pub const CLOCK_RATE_VIDEO_HZ: u32 = 90_000;

/// Audio RTP clock (Hz) — `a=rtpmap:0 PCMU/8000` (plan lock).
pub const CLOCK_RATE_AUDIO_HZ: u32 = 8_000;

/// Loop tick — divides the 5 s interval so teardown stops are prompt and the
/// first SR after `PLAY` is emitted immediately (`last_sent = None`).
const TICK_MS: u64 = 250;

// ---------------------------------------------------------------------------
// Per-track state + counters
// ---------------------------------------------------------------------------

/// One track's RTCP-relevant state: the values an SR must report, kept in
/// step with what is actually on the wire.
///
/// **Counters own here** (module-level per the plan's Todo 13 "Do" row —
/// the Todo 12 payload tracks expose only `seq()/rtptime()/ssrc()`, which
/// already exist as getters; no payload file is modified). Feed the wire with
/// [`counting_sink`], which parses the 12-byte RTP header of every forwarded
/// packet — the SR then reports *exactly* what the receiver saw, never a
/// parallel bookkeeping figure that could drift.
///
/// `packets`/`octets` are RFC 3550 cumulative counts: 32-bit, may wrap, must
/// not be reset mid-session (RFC 3550 §6.4.1 explicitly allows the wrap and
/// expects receivers to handle it) — additions use `wrapping_add`, never `+`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RtState {
    /// SSRC — constant for the session (Todo 12 tracks own the same value).
    pub ssrc: u32,
    /// Latest observed RTP sequence number.
    pub seq: u16,
    /// Latest observed RTP timestamp (mod 2³² — see the wrap section).
    pub rtptime: u32,
    /// Cumulative RTP packets sent (wrapping u32, RFC 3550).
    pub packets: u32,
    /// Cumulative payload octets sent, RTP header excluded (wrapping u32).
    pub octets: u32,
}

impl RtState {
    /// Fresh track state for an SSRC, before any packet has been observed.
    pub fn with_ssrc(ssrc: u32) -> Self {
        Self {
            ssrc,
            ..Self::default()
        }
    }

    /// Observe one on-the-wire RTP packet: parse `seq`/`rtptime`/`ssrc` from
    /// the fixed 12-byte header and add `len − 12` payload octets to the
    /// cumulative count. Returns `false` (state untouched) for packets
    /// shorter than a header — defensive, never panics, never chokes.
    pub fn observe_rtp_packet(&mut self, packet: &[u8]) -> bool {
        if packet.len() < 12 {
            return false;
        }
        self.seq = u16::from_be_bytes([packet[2], packet[3]]);
        self.rtptime = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]);
        self.ssrc = u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]);
        self.packets = self.packets.wrapping_add(1);
        let payload = (packet.len() - 12) as u32;
        self.octets = self.octets.wrapping_add(payload);
        true
    }
}

/// Shared handle between the RTP path (writer) and the SR loop (reader).
pub type SharedRtState = Arc<Mutex<RtState>>;

/// New shared track state seeded with an SSRC.
pub fn shared_state(ssrc: u32) -> SharedRtState {
    Arc::new(Mutex::new(RtState::with_ssrc(ssrc)))
}

/// Adapter for the RTP path: forward a packet to the real sink
/// (`RtpSender::send_rtp` on device) and, **only if the send succeeds**,
/// update the shared [`RtState`]. Matches Todo 12's
/// `RtpSink<'a> = &'a mut dyn FnMut(&[u8]) -> io::Result<()>` shape, so
/// wiring is:
///
/// ```ignore
/// let sink = rtcp::counting_sink(state.clone(), move |p| sender.send_rtp(p));
/// track.packetize(jpeg, &mut sink);
/// ```
pub fn counting_sink<F>(state: SharedRtState, forward: F) -> impl FnMut(&[u8]) -> io::Result<()>
where
    F: Fn(&[u8]) -> io::Result<()>,
{
    move |packet: &[u8]| {
        forward(packet)?;
        if let Ok(mut guard) = state.lock() {
            guard.observe_rtp_packet(packet);
        }
        // Poisoned mutex: a panic elsewhere must not kill the stream — the
        // forward already succeeded, so report success.
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Sender Report builder (RFC 3550 §6.4.1)
// ---------------------------------------------------------------------------

/// The SR content, exactly the RFC 3550 §6.4.1 field order. Serialized by
/// [`SenderReport::to_bytes`] — hand-rolled, zero new crates (plan lock).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SenderReport {
    /// SSRC of sender (4 bytes).
    pub ssrc: u32,
    /// NTP timestamp, most significant word — seconds since 1900-01-01.
    pub ntp_seconds: u32,
    /// NTP timestamp, least significant word — fractional seconds × 2³².
    pub ntp_fraction: u32,
    /// RTP timestamp corresponding to the NTP time (track counter, mod 2³²).
    pub rtp_timestamp: u32,
    /// Sender's packet count (wrapping u32).
    pub packet_count: u32,
    /// Sender's octet count — payload octets, header excluded (wrapping u32).
    pub octet_count: u32,
}

impl SenderReport {
    /// Serialize to the wire: 28 bytes, big-endian, V=2/P=0/RC=0/PT=200/
    /// length=6 as the first four bytes.
    pub fn to_bytes(&self) -> [u8; SR_WIRE_LEN] {
        let mut b = [0u8; SR_WIRE_LEN];
        b[0] = 0b10_0_00000; // V=2, P=0, RC=0 (a Sender Report has no report blocks)
        b[1] = PT_SR; // 200
        b[2..4].copy_from_slice(&SR_LENGTH_FIELD.to_be_bytes());
        b[4..8].copy_from_slice(&self.ssrc.to_be_bytes());
        b[8..12].copy_from_slice(&self.ntp_seconds.to_be_bytes());
        b[12..16].copy_from_slice(&self.ntp_fraction.to_be_bytes());
        b[16..20].copy_from_slice(&self.rtp_timestamp.to_be_bytes());
        b[20..24].copy_from_slice(&self.packet_count.to_be_bytes());
        b[24..28].copy_from_slice(&self.octet_count.to_be_bytes());
        b
    }
}

/// Map a `SystemTime` to the 64-bit NTP timestamp (seconds, fraction) —
/// the **only** NTP encoder in the firmware.
///
/// - seconds = unix_seconds + [`NTP_UNIX_EPOCH_OFFSET_SECS`], truncated to
///   the 32-bit NTP word (era 0; the 2036 wrap point is documented, and 2026
///   dates are far from it).
/// - fraction = `nanos × 2³² / 10⁹` (floor), computed in u64 without
///   overflow (`10⁹ × 2³² < u64::MAX`).
/// - Inputs before 1970 (impossible from SNTP) clamp to the epoch instead of
///   underflowing — they still map to a *real* NTP time (1970-01-01), never a
///   fabricated one.
pub fn system_time_to_ntp(t: SystemTime) -> (u32, u32) {
    let since_unix = t.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    let secs = since_unix.as_secs().saturating_add(NTP_UNIX_EPOCH_OFFSET_SECS);
    let fraction = (u64::from(since_unix.subsec_nanos()) << 32) / 1_000_000_000;
    (secs as u32, fraction as u32)
}

/// Build the SR for a track at a **real** wallclock (the caller has already
/// passed the [`sr_action`] gate — `wallclock` must come from
/// [`SrClock::poll`], i.e. from `net::last_sync_utc()`, never from a guess).
pub fn build_sender_report(track: &RtState, wallclock: SystemTime) -> SenderReport {
    let (ntp_seconds, ntp_fraction) = system_time_to_ntp(wallclock);
    SenderReport {
        ssrc: track.ssrc,
        ntp_seconds,
        ntp_fraction,
        rtp_timestamp: track.rtptime,
        packet_count: track.packets,
        octet_count: track.octets,
    }
}

// ---------------------------------------------------------------------------
// Explicit 32-bit wrap arithmetic (plan MUST DO)
// ---------------------------------------------------------------------------

/// Forward distance between two 32-bit RTP timestamps **on the 2³² ring**.
///
/// This is the plan's "make the 32-bit timestamp wrap explicit" made literal:
/// wrap handling is a named, documented function call, not operator overflow
/// luck. `wrapping_sub` is the definition of forward modular distance —
/// `rtp_delta_units(0xFFFF_FFFF, 0) == 1`, not `0u32.wrapping_sub` surprises
/// and never `-` (which would panic in debug builds exactly at the boundary
/// that matters).
///
/// One unit = 1/90 000 s for video, 1/8 000 s for audio; the wrap period is
/// 2³²/90 000 ≈ 47 721 s ≈ 13.3 h (plan reference) and 2³²/8 000 ≈ 149 131 s
/// ≈ 41.4 h for audio.
pub fn rtp_delta_units(prev: u32, now: u32) -> u32 {
    now.wrapping_sub(prev)
}

/// Elapsed **microseconds** between two RTP timestamps at `clock_rate_hz`,
/// correct across the 32-bit wrap (see [`rtp_delta_units`]).
///
/// Saturates safely: max distance 2³²−1 units × 10⁶ fits u64 with room to
/// spare; `clock_rate_hz == 0` (programming error) returns 0 rather than
/// dividing by zero.
pub fn rtp_delta_us(prev: u32, now: u32, clock_rate_hz: u32) -> u64 {
    if clock_rate_hz == 0 {
        return 0;
    }
    u64::from(rtp_delta_units(prev, now)) * 1_000_000 / u64::from(clock_rate_hz)
}

/// Cumulative elapsed RTP units in a **u64**, fed 32-bit timestamps: the
/// "reported time is continuous across the `0xFFFF_FFFF → 0` boundary"
/// assertion target. Each `push` adds the modular forward distance
/// ([`rtp_delta_units`]), so the total never jumps backwards or skips at the
/// wrap — unlike naive `now - prev` arithmetic, which goes negative (or
/// wraps) exactly there.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RtpElapsed {
    last: Option<u32>,
    total_units: u64,
}

impl RtpElapsed {
    /// No timestamps observed yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold in the latest 32-bit RTP timestamp; returns the cumulative
    /// forward distance in RTP units (0 for the first sample).
    pub fn push(&mut self, timestamp: u32) -> u64 {
        if let Some(prev) = self.last {
            self.total_units += u64::from(rtp_delta_units(prev, timestamp));
        }
        self.last = Some(timestamp);
        self.total_units
    }

    /// Cumulative units since the first sample (u64 — wraps cannot occur in
    /// practice: 2³² samples would first have to arrive in one session).
    pub fn total_units(&self) -> u64 {
        self.total_units
    }

    /// Cumulative units expressed in microseconds at `clock_rate_hz`.
    pub fn elapsed_us(&self, clock_rate_hz: u32) -> u64 {
        if clock_rate_hz == 0 {
            return 0;
        }
        self.total_units * 1_000_000 / u64::from(clock_rate_hz)
    }
}

// ---------------------------------------------------------------------------
// The 5 s scheduler (pure decision fn)
// ---------------------------------------------------------------------------

/// Is an SR due at `now_ms`? Pure and thread-free — host-tested directly.
///
/// - `last_sent_ms = None` → always due (first SR of the session, and the
///   forced report on session teardown).
/// - Otherwise due only once ≥ [`SR_INTERVAL_MS`] has elapsed — **never
///   early** (4 999 ms after the last SR is not due; 5 000 ms is).
/// - `saturating_sub` means a monotonic-clock anomaly can under-report
///   elapsed time but can never manufacture an early send.
pub fn sr_due(now_ms: u64, last_sent_ms: Option<u64>) -> bool {
    match last_sent_ms {
        None => true,
        Some(last) => now_ms.saturating_sub(last) >= SR_INTERVAL_MS,
    }
}

// ---------------------------------------------------------------------------
// Suppression gate + wallclock anchor (never fabricate)
// ---------------------------------------------------------------------------

/// The exact suppression line carried by `SrAction::Suppress`
/// (= [`SUPPRESSED_LOG`]; kept as a variant payload so tests assert the
/// emitted text without capturing stdout).
pub const _SUPPRESSED_MSG: &str = SUPPRESSED_LOG;

/// What Todo 14's SNTP exposes to this module. Injected as a closure by
/// [`spawn_sender_reports`] (`|| SyncSample { may_send_rtcp:
/// net::may_send_rtcp(), last_sync_utc: net::last_sync_utc() }`) so the
/// module compiles standalone on the host while the device path reads only
/// the plan-sanctioned getters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncSample {
    /// `net::may_send_rtcp()` — the plan's suppression predicate.
    pub may_send_rtcp: bool,
    /// `net::last_sync_utc()` — the only permitted wallclock source.
    pub last_sync_utc: Option<SystemTime>,
}

impl SyncSample {
    /// Construct a sample (device glue and host tests alike).
    pub const fn new(may_send_rtcp: bool, last_sync_utc: Option<SystemTime>) -> Self {
        Self {
            may_send_rtcp,
            last_sync_utc,
        }
    }

    /// Never-synced / unsynced state — SRs must be suppressed.
    pub const fn unsynced() -> Self {
        Self {
            may_send_rtcp: false,
            last_sync_utc: None,
        }
    }
}

/// Wallclock for SRs, anchored on the SNTP sync value and advanced by
/// monotonic milliseconds.
///
/// Behaviour (every branch is host-tested):
///
/// - `may_send_rtcp == false` **or** `last_sync_utc == None` → `None`
///   (suppression — the anchor is cleared so a later resync re-anchors).
/// - First observation of a sync value → returns that value verbatim and
///   anchors the monotonic origin here.
/// - Subsequent polls with the same sync value → `sync + (mono_ms − anchor)`.
///   Monotonic time only *advances the SNTP value*; it is never reported on
///   its own.
/// - A changed sync value (60-minute resync) → re-anchor at the new value,
///   which is what keeps reported NTP within one poll interval of
///   `last_sync_utc` (plan acceptance) and self-corrects accumulated drift.
#[derive(Clone, Debug, Default)]
pub struct SrClock {
    anchored_sync: Option<SystemTime>,
    anchored_mono_ms: u64,
}

impl SrClock {
    /// No anchor yet (equivalent to unsynced until the first synced poll).
    pub const fn new() -> Self {
        Self {
            anchored_sync: None,
            anchored_mono_ms: 0,
        }
    }

    /// The wallclock an SR may carry right now, or `None` → suppress.
    ///
    /// `mono_ms` is any monotonic millisecond counter (device: the spawn
    /// loop's `Instant`-derived tick; host tests: fabricated values).
    pub fn poll(&mut self, sample: SyncSample, mono_ms: u64) -> Option<SystemTime> {
        if !sample.may_send_rtcp {
            self.anchored_sync = None;
            return None;
        }
        let sync = sample.last_sync_utc?;
        if self.anchored_sync != Some(sync) {
            self.anchored_sync = Some(sync);
            self.anchored_mono_ms = mono_ms;
            return Some(sync);
        }
        Some(sync + Duration::from_millis(mono_ms.saturating_sub(self.anchored_mono_ms)))
    }
}

/// One scheduler decision for one track — the pure core the spawn loop and
/// the host tests share.
///
/// Gate order is deliberate (the plan's cardinal rule first):
///
/// 1. Unsynced (`!may_send_rtcp`) **or** no wallclock →
///    `SrAction::Suppress(SUPPRESSED_LOG)` — regardless of how overdue the SR
///    is. No bytes are ever produced on this branch.
/// 2. Synced but < [`SR_INTERVAL_MS`] since the last SR → `Wait`.
/// 3. Otherwise → `Send` with the exact 28-byte RFC 3550 §6.4.1 packet.
///
/// `wallclock` must be [`SrClock`]'s output; passing `Some(_)` alongside
/// `may_send_rtcp = false` still suppresses (belt and braces: the predicate
/// wins).
pub fn sr_action(
    now_ms: u64,
    last_sent_ms: Option<u64>,
    sample: &SyncSample,
    wallclock: Option<SystemTime>,
    track: &RtState,
) -> SrAction {
    if !sample.may_send_rtcp {
        return SrAction::Suppress(SUPPRESSED_LOG);
    }
    let Some(wallclock) = wallclock else {
        // Synced but no wallclock yet — impossible via SrClock, defended
        // anyway: sending nothing is always safe, fabricating never is.
        return SrAction::Suppress(SUPPRESSED_LOG);
    };
    if !sr_due(now_ms, last_sent_ms) {
        return SrAction::Wait;
    }
    SrAction::Send(build_sender_report(track, wallclock).to_bytes().to_vec())
}

/// The outcome of one [`sr_action`] evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SrAction {
    /// These bytes go on the wire now (28-byte SR).
    Send(Vec<u8>),
    /// SNTP unsynced — suppressed; payload is the exact log line to emit.
    Suppress(&'static str),
    /// Synced and not yet due — emit nothing, log nothing.
    Wait,
}

// ---------------------------------------------------------------------------
// Incoming RTCP (Receiver Reports) — parse, don't choke
// ---------------------------------------------------------------------------

/// Why a compound RTCP packet could not be walked. Typed (house style) —
/// never a panic; the caller logs the description and drops the packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RtcpParseError {
    /// Zero-length datagram.
    Empty,
    /// A sub-packet's version bits were not 2 (RFC 3550 §6: V=2 required).
    BadVersion {
        /// The version bits found (top 2 bits of byte 0).
        version: u8,
    },
    /// A sub-packet's `length` field ran past the datagram end.
    Truncated {
        /// Bytes this sub-packet needs (header + payload).
        needed: usize,
        /// Bytes actually available from its start.
        have: usize,
    },
}

impl fmt::Display for RtcpParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "empty RTCP datagram"),
            Self::BadVersion { version } => {
                write!(f, "RTCP version {version} != 2")
            }
            Self::Truncated { needed, have } => {
                write!(f, "RTCP sub-packet truncated: need {needed} bytes, have {have}")
            }
        }
    }
}

impl std::error::Error for RtcpParseError {}

/// One sub-packet of a compound RTCP datagram, summarised just far enough
/// "to not choke and to log" (plan Todo 13). Full RR semantics (loss,
/// jitter, DLSR) are deliberately **not** modelled — see the module header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RtcpSummary {
    /// PT 200 — a Sender Report (from the other side).
    SenderReport {
        /// SSRC of that sender.
        ssrc: u32,
    },
    /// PT 201 — a Receiver Report: parsed for header integrity and logging.
    ReceiverReport {
        /// SSRC of the report's sender (us, normally).
        ssrc: u32,
        /// RC field: number of report blocks (0–31).
        blocks: u8,
    },
    /// Anything else (SDES BYE APP / unknown PT) — walked past safely.
    Other {
        /// The packet type byte.
        pt: u8,
        /// Sender SSRC if the sub-packet is long enough to carry one.
        ssrc: Option<u32>,
    },
}

/// Walk a (possibly compound) RTCP datagram: validate V=2 on every
/// sub-packet, honour each `length` field, bounds-check everything. Returns
/// summaries in order; the first violation is a typed [`RtcpParseError`].
///
/// This is the "handle incoming RTCP Receiver Reports" surface: parse →
/// [`describe_rtcp`] → log/forward. Loss/jitter adaptation is out of scope
/// (documented), so a well-formed RR never changes behaviour — it just never
/// chokes the reader either.
pub fn parse_rtcp(packet: &[u8]) -> Result<Vec<RtcpSummary>, RtcpParseError> {
    if packet.is_empty() {
        return Err(RtcpParseError::Empty);
    }
    let mut out = Vec::new();
    let mut off = 0usize;
    while off < packet.len() {
        let have = packet.len() - off;
        if have < 4 {
            return Err(RtcpParseError::Truncated { needed: 4, have });
        }
        let b0 = packet[off];
        let version = b0 >> 6;
        if version != 2 {
            return Err(RtcpParseError::BadVersion { version });
        }
        let pt = packet[off + 1];
        let length_words = u16::from_be_bytes([packet[off + 2], packet[off + 3]]) as usize;
        let total = (length_words + 1) * 4;
        if have < total {
            return Err(RtcpParseError::Truncated { needed: total, have });
        }
        let ssrc = (total >= 8).then(|| {
            u32::from_be_bytes([packet[off + 4], packet[off + 5], packet[off + 6], packet[off + 7]])
        });
        let blocks = b0 & 0x1F; // RC field
        out.push(match pt {
            PT_SR => RtcpSummary::SenderReport {
                ssrc: ssrc.unwrap_or(0),
            },
            PT_RR => RtcpSummary::ReceiverReport {
                ssrc: ssrc.unwrap_or(0),
                blocks,
            },
            _ => RtcpSummary::Other { pt, ssrc },
        });
        off += total;
    }
    Ok(out)
}

/// Log line for a parsed compound packet — the "forward/log" half of the
/// plan's RR requirement. States the deliberate scope limit in the line
/// itself so a future reader of a serial log sees why an RR changed nothing.
pub fn describe_rtcp(packets: &[RtcpSummary]) -> String {
    if packets.is_empty() {
        return "rtcp: (no packets)".to_string();
    }
    let parts: Vec<String> = packets
        .iter()
        .map(|p| match p {
            RtcpSummary::SenderReport { ssrc } => format!("SR ssrc=0x{ssrc:08x}"),
            RtcpSummary::ReceiverReport { ssrc, blocks } => {
                format!("RR ssrc=0x{ssrc:08x} blocks={blocks}")
            }
            RtcpSummary::Other { pt, ssrc } => match ssrc {
                Some(s) => format!("PT={pt} ssrc=0x{s:08x}"),
                None => format!("PT={pt}"),
            },
        })
        .collect();
    format!(
        "rtcp: {} — receiver-report semantics logged only (loss/jitter/delay not acted on)",
        parts.join(", ")
    )
}

// ---------------------------------------------------------------------------
// The device loop
// ---------------------------------------------------------------------------

/// Stop switch for a running SR loop (cloned by the caller, e.g. the session
/// teardown path).
#[derive(Clone, Debug)]
pub struct SrLoop {
    stop: Arc<AtomicBool>,
}

impl SrLoop {
    /// Request loop exit; the loop then emits the **final** SRs (plan: "and
    /// on session teardown") and returns.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Whether stop has been requested.
    pub fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

/// Spawn the per-track Sender-Report loop (plan Todo 13 "Do": per track, at
/// least every 5 s, and on session teardown).
///
/// - `video` / `audio` — shared [`RtState`] kept in step by
///   [`counting_sink`] (initialised with the SSRC of each Todo 12 track).
/// - `video_rtcp` / `audio_rtcp` — where SR bytes go. On device these wrap
///   Todo 11's `RtpSender::send_rtcp` (interleaved framing on the track's
///   RTCP channel is `RtpSender`'s job, not this module's); on the host the
///   tests inject capturing closures. Bounds are `Fn`, `Send`, `Sync`,
///   `'static` so they can cross into the thread.
/// - `sync` — the SNTP sample source; device glue passes
///   `move || rtcp::SyncSample { may_send_rtcp: net::may_send_rtcp(),
///   last_sync_utc: net::last_sync_utc() }`.
///
/// Loop shape: an immediate first tick (`last_sent = None` → due), then every
/// [`TICK_MS`] re-evaluate both tracks; suppression logs at most once per
/// 5 s window (no unsynced spam); on [`SrLoop::stop`] both tracks get a
/// forced final decision (`last_sent = None`) and the thread exits.
///
/// Wiring note (documented): the actual `RtpSender` acquisition and the
/// `SyncSample` closure over `net::` belong to Todo 20's boot/session
/// wiring — this helper is interface-ready but not called from `main.rs`
/// yet (the module is `#[allow(dead_code)]` there for exactly that reason).
pub fn spawn_sender_reports<VR, AR, SC>(
    video: SharedRtState,
    audio: SharedRtState,
    video_rtcp: VR,
    audio_rtcp: AR,
    sync: SC,
) -> io::Result<(SrLoop, JoinHandle<()>)>
where
    VR: Fn(&[u8]) -> io::Result<()> + Send + Sync + 'static,
    AR: Fn(&[u8]) -> io::Result<()> + Send + Sync + 'static,
    SC: Fn() -> SyncSample + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let handle = SrLoop {
        stop: Arc::clone(&stop),
    };
    let builder = thread::Builder::new().name("rtcp-sr".to_string());
    let thread = builder.spawn(move || {
        let start = Instant::now();
        let mut clock = SrClock::new();
        let mut last_sent: [Option<u64>; 2] = [None, None];
        let mut last_suppressed_log: [Option<u64>; 2] = [None, None];
        let video_dyn: &dyn Fn(&[u8]) -> io::Result<()> = &video_rtcp;
        let audio_dyn: &dyn Fn(&[u8]) -> io::Result<()> = &audio_rtcp;
        loop {
            let now_ms = start.elapsed().as_millis() as u64;
            let sample = sync();
            let wallclock = clock.poll(sample.clone(), now_ms);
            let stopped = stop.load(Ordering::SeqCst);
            for (i, state, sink) in [
                (0usize, &video, video_dyn),
                (1usize, &audio, audio_dyn),
            ] {
                // On stop: force the final teardown SR (last_sent = None →
                // sr_due is true → the plan's "and on session teardown").
                let due_from = if stopped { None } else { last_sent[i] };
                let action = {
                    let guard = state.lock().unwrap_or_else(|p| p.into_inner());
                    sr_action(now_ms, due_from, &sample, wallclock, &guard)
                }; // release the lock before any socket write
                apply_action(
                    action,
                    sink,
                    &mut last_sent[i],
                    &mut last_suppressed_log[i],
                    now_ms,
                );
            }
            if stopped {
                break;
            }
            thread::sleep(Duration::from_millis(TICK_MS));
        }
    })?;
    Ok((handle, thread))
}

/// Execute one decision: send + log + stamp, or log the suppression line at
/// most once per interval, or do nothing.
fn apply_action(
    action: SrAction,
    sink: &dyn Fn(&[u8]) -> io::Result<()>,
    last_sent: &mut Option<u64>,
    last_suppressed_log: &mut Option<u64>,
    now_ms: u64,
) {
    match action {
        SrAction::Send(bytes) => match sink(&bytes) {
            Ok(()) => {
                println!(
                    "rtcp: SR sent ({} bytes, ssrc=0x{:08x}, rtp_ts={})",
                    bytes.len(),
                    u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
                    u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]),
                );
                *last_sent = Some(now_ms);
            }
            Err(e) => {
                // Keep last_sent: the interval restarts only on a successful
                // send, so a dead connection retries each tick with a log
                // line instead of going silent for 5 s.
                println!("rtcp: SR send failed: {e}");
            }
        },
        SrAction::Suppress(line) => {
            if sr_due(now_ms, *last_suppressed_log) {
                println!("rtcp: {line}");
                *last_suppressed_log = Some(now_ms);
            }
        }
        SrAction::Wait => {}
    }
}

// ---------------------------------------------------------------------------
// In-file tests (also host-executed via harness/tests/task13_rtcp.rs)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sr_layout_is_rfc3550_6_4_1_byte_exact() {
        let sr = SenderReport {
            ssrc: 0x1122_3344,
            ntp_seconds: 0xAABB_CCDD,
            ntp_fraction: 0x0102_0304,
            rtp_timestamp: 0x5566_7788,
            packet_count: 0x0000_0009,
            octet_count: 0x0000_0BB8, // 3000 payload octets
        };
        let b = sr.to_bytes();
        assert_eq!(b.len(), SR_WIRE_LEN);
        assert_eq!(b[0], 0x80, "V=2, P=0, RC=0");
        assert_eq!(b[1], 200, "PT=200");
        assert_eq!(&b[2..4], &[0x00, 0x06], "length = 6 words - 1");
        assert_eq!(&b[4..8], &0x1122_3344u32.to_be_bytes());
        assert_eq!(&b[8..12], &0xAABB_CCDDu32.to_be_bytes(), "NTP ms-word");
        assert_eq!(&b[12..16], &0x0102_0304u32.to_be_bytes(), "NTP ls-word");
        assert_eq!(&b[16..20], &0x5566_7788u32.to_be_bytes(), "RTP ts");
        assert_eq!(&b[20..24], &9u32.to_be_bytes(), "packet count");
        assert_eq!(&b[24..28], &3000u32.to_be_bytes(), "octet count");
    }

    #[test]
    fn ntp_maps_known_instants() {
        // Era-2000 anchor: 2000-01-01T00:00:00Z = 946 684 800 unix s.
        let y2k = UNIX_EPOCH + Duration::from_secs(946_684_800);
        assert_eq!(
            system_time_to_ntp(y2k),
            (3_155_673_600, 0),
            "2000-01-01 = 946684800 + 2208988800 NTP seconds, zero fraction"
        );
        assert_eq!(NTP_UNIX_EPOCH_OFFSET_SECS, 2_208_988_800);
        // Half a second is exactly 2^31 in the fraction word.
        let half = UNIX_EPOCH + Duration::new(1_000_000, 500_000_000);
        assert_eq!(system_time_to_ntp(half), (1_000_000 + 2_208_988_800, 0x8000_0000));
    }

    #[test]
    fn wrap_is_explicit_and_time_is_continuous() {
        assert_eq!(rtp_delta_units(0xFFFF_FFFF, 0), 1, "0xFFFF_FFFF -> 0 is +1");
        assert_eq!(rtp_delta_units(0xFFFF_FFF0, 0x0000_000F), 0x1F);
        assert_eq!(rtp_delta_us(0xFFFF_FFFF, 0, CLOCK_RATE_VIDEO_HZ), 11);

        let mut elapsed = RtpElapsed::new();
        assert_eq!(elapsed.push(0xFFFF_FFF0), 0);
        assert_eq!(elapsed.push(0xFFFF_FFFA), 10);
        // The boundary itself: forward distance 10 units, not a negative jump.
        assert_eq!(elapsed.push(0x0000_0004), 20);
        assert_eq!(elapsed.push(0x0000_0014), 36);
        assert_eq!(elapsed.total_units(), 36, "reported time continuous across wrap");
    }

    #[test]
    fn scheduler_fires_at_five_seconds_not_before() {
        assert!(sr_due(0, None), "first SR of a session is always due");
        assert!(!sr_due(4_999, Some(0)), "never early");
        assert!(sr_due(5_000, Some(0)), "exactly 5 s");
        assert!(!sr_due(5_999, Some(1_000)), "4 999 ms elapsed is not due");
        assert!(sr_due(6_000, Some(1_000)), "5 000 ms elapsed is due");
        assert!(sr_due(u64::MAX, Some(0)), "saturation can only delay");
    }

    #[test]
    fn unsynced_suppresses_with_the_exact_plan_log_line() {
        let sample = SyncSample::unsynced();
        let mut clock = SrClock::new();
        assert_eq!(clock.poll(sample.clone(), 0), None, "never fabricates");
        let track = RtState::with_ssrc(1);
        let action = sr_action(9_999_999, Some(0), &sample, None, &track);
        assert_eq!(action, SrAction::Suppress(SUPPRESSED_LOG));
        assert_eq!(SUPPRESSED_LOG, "unsynced, SR suppressed");
        match action {
            SrAction::Suppress(line) => assert_eq!(line, "unsynced, SR suppressed"),
            _ => panic!("unsynced must suppress, got {action:?}"),
        }
    }

    #[test]
    fn synced_clock_advances_from_the_sntp_anchor() {
        let sync = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let sample = SyncSample::new(true, Some(sync));
        let mut clock = SrClock::new();
        assert_eq!(clock.poll(sample.clone(), 0), Some(sync), "anchor = SNTP value");
        assert_eq!(
            clock.poll(sample.clone(), 5_000),
            Some(sync + Duration::from_millis(5_000)),
            "monotonic only advances the SNTP value"
        );
        // Resync (60-min poll) re-anchors at the new sync value.
        let sync2 = sync + Duration::from_secs(3_600);
        let sample2 = SyncSample::new(true, Some(sync2));
        assert_eq!(clock.poll(sample2, 10_000), Some(sync2));
        // Losing sync clears the anchor: no wallclock, suppressed afterwards.
        assert_eq!(clock.poll(SyncSample::unsynced(), 11_000), None);
        assert_eq!(clock.poll(SyncSample::unsynced(), 12_000), None);
    }

    #[test]
    fn receiver_report_parses_and_malformed_is_typed_error() {
        // Compound: our own SR + a 1-block RR, V=2 throughout.
        let mut pkt = Vec::new();
        pkt.extend_from_slice(&SenderReport {
            ssrc: 0xCAFE_0001,
            ntp_seconds: 1,
            ntp_fraction: 0,
            rtp_timestamp: 2,
            packet_count: 3,
            octet_count: 4,
        }
        .to_bytes());
        // RR: V=2, RC=1, PT=201, length=7 (32 bytes total), ssrc + one block.
        pkt.extend_from_slice(&[0x81, 201, 0x00, 0x07]);
        pkt.extend_from_slice(&0xCAFE_0002u32.to_be_bytes());
        pkt.extend_from_slice(&[0u8; 24]); // one report block
        let parsed = parse_rtcp(&pkt).expect("compound parses");
        assert_eq!(parsed.len(), 2);
        assert_eq!(
            parsed[0],
            RtcpSummary::SenderReport { ssrc: 0xCAFE_0001 }
        );
        assert_eq!(
            parsed[1],
            RtcpSummary::ReceiverReport {
                ssrc: 0xCAFE_0002,
                blocks: 1
            }
        );
        let line = describe_rtcp(&parsed);
        assert!(line.contains("RR ssrc=0xcafe0002 blocks=1"), "{line}");
        assert!(line.contains("logged only"), "{line}");

        assert_eq!(parse_rtcp(&[]), Err(RtcpParseError::Empty));
        assert_eq!(
            parse_rtcp(&[0x40, 201, 0x00, 0x00]), // V=1
            Err(RtcpParseError::BadVersion { version: 1 })
        );
        assert!(matches!(
            parse_rtcp(&[0x80, 201, 0x00, 0x0E]), // claims 60 bytes, has 4
            Err(RtcpParseError::Truncated { .. })
        ));
    }
}