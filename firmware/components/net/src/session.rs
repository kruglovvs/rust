//! Session lifecycle and backpressure policy (plan Todo 14, load-bearing
//! behaviours (b), (c) and (d)).
//!
//! Plan contract, quoted from `.omo/plans/esp32s3-av-stream-and-sd-recording.md`
//! Todo 14 "References":
//!
//! > (b) **pause capture when DMA is short** — below 12 KB free or a 4 KB
//! > largest block, back off rather than pushing on; (c) **capture only while
//! > a client is connected**, and honour a server-provided cooldown so a slow
//! > client slows the camera instead of exhausting the heap; (d) **tear the
//! > server down from the disconnect callback**.
//!
//! This file is deliberately **pure `std`** — no `esp_idf_*` imports — so its
//! decision logic and state machine run as host-executed tests via the
//! `harness/tests/task14_net.rs` `#[path]` include (the `audio_quality` /
//! `recorder` pattern). Device glue (`net.rs`) samples the heap and feeds
//! events here; the RTSP layer (Todo 11) raises [`Event::ClientConnected`] /
//! [`Event::ClientDisconnected`] from its disconnect callback and MUST apply
//! the returned [`SessionAction::TearDownServer`] — behaviour (d).
//!
//! Decision precedence (behaviour (b)/(c)):
//! 1. no client connected ([`SessionState`] != [`SessionState::Active`]) →
//!    [`CaptureDecision::Suspend`] — capture is off entirely (c);
//! 2. inside the server-provided cooldown armed by
//!    [`Session::note_write_stalled`] → [`CaptureDecision::Backoff`] (c);
//! 3. DMA free < 12 KiB OR DMA largest block < 4 KiB → [`CaptureDecision::Backoff`] (b).
//!
//! Backoff drops the frame (never queues it): a stalled client slows the
//! camera, the heap stays flat (plan QA — failure row for Todo 14).

use std::sync::Mutex;

/// Behaviour (b): DMA free below this many bytes → back off.
/// Plan: "below 12 KB free or a 4 KB largest block".
pub const DMA_FREE_MIN_BYTES: usize = 12 * 1024;

/// Behaviour (b): DMA largest free block below this many bytes → back off.
pub const DMA_LARGEST_MIN_BYTES: usize = 4 * 1024;

/// Behaviour (c): default server-provided cooldown after a slow-client write
/// stall, in milliseconds. The RTSP layer (Todo 11) may override per session
/// via [`Session::set_cooldown_ms`]. 500 ms = five frame periods at the
/// proven 10 fps operating point: long enough to let a wedged TCP window
/// drain, short enough that a recovering client sees motion again quickly.
pub const DEFAULT_COOLDOWN_MS: u64 = 500;

/// Lifecycle of one RTSP client session.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SessionState {
    /// No client has connected (or the post-teardown idle state).
    Idle,
    /// A client is connected; capture runs.
    Active,
    /// The session was torn down (client disconnect or Wi-Fi loss);
    /// waiting for the next connection or a Wi-Fi restore.
    TornDown,
}

/// Events fed to [`Session::handle`] by the RTSP layer (Todo 11) and the
/// Wi-Fi supervisor (`net.rs`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A client completed SETUP/PLAY — capture may start.
    ClientConnected,
    /// The RTSP disconnect callback fired. Handling this returns
    /// [`SessionAction::TearDownServer`] (behaviour (d): teardown happens
    /// *from* the disconnect callback, not later on a poll).
    ClientDisconnected,
    /// The Wi-Fi link dropped (AP killed, association lost).
    WifiLost,
    /// The Wi-Fi link is back and the station has an IP again.
    WifiRestored,
}

/// Effects the caller must apply, in order, as returned by
/// [`Session::handle`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SessionAction {
    /// Start / resume the capture loop (frame grabber may run).
    StartCapture,
    /// Stop the capture loop and drop any queued frame (never hold frames).
    StopCapture,
    /// Tear the RTSP server/session resources down **now**, from the
    /// caller's disconnect callback (behaviour (d)).
    TearDownServer,
}

/// Why capture is backing off instead of grabbing a frame.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BackoffReason {
    /// DMA free bytes below [`DMA_FREE_MIN_BYTES`] (behaviour (b)).
    DmaFreeLow { free: usize },
    /// DMA largest block below [`DMA_LARGEST_MIN_BYTES`] (behaviour (b)).
    DmaLargestLow { largest: usize },
    /// Inside the slow-client cooldown armed by
    /// [`Session::note_write_stalled`] (behaviour (c)).
    Cooldown { until_ms: u64 },
}

/// Result of the per-tick capture policy (behaviour (b) + (c)).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CaptureDecision {
    /// Grab the next frame.
    Capture,
    /// Skip this tick; drop the frame, do not queue it.
    Backoff(BackoffReason),
    /// No client — capture fully suspended.
    Suspend,
}

/// One session state machine. Cheap and `const`-constructible so it can live
/// in a module-level [`static SESSION`] that the RTSP layer (Todo 11) and the
/// Wi-Fi supervisor (`net.rs`) share.
pub struct Session {
    state: SessionState,
    cooldown_ms: u64,
    cooldown_until_ms: u64,
}

impl Session {
    pub const fn new(cooldown_ms: u64) -> Self {
        Self {
            state: SessionState::Idle,
            cooldown_ms,
            cooldown_until_ms: 0,
        }
    }

    pub const fn with_defaults() -> Self {
        Self::new(DEFAULT_COOLDOWN_MS)
    }

    pub fn state(&self) -> SessionState {
        self.state
    }

    /// Server-provided cooldown currently configured (ms).
    pub fn cooldown_ms(&self) -> u64 {
        self.cooldown_ms
    }

    /// Override the cooldown — the RTSP layer "provides" it per plan (c).
    pub fn set_cooldown_ms(&mut self, cooldown_ms: u64) {
        self.cooldown_ms = cooldown_ms;
    }

    /// Absolute monotonic time (ms) until which capture backs off.
    pub fn cooldown_until_ms(&self) -> u64 {
        self.cooldown_until_ms
    }

    /// Behaviour (c): a slow client (write would block / TCP window closed)
    /// arms the cooldown; until `now_ms` passes `cooldown_until_ms`, the
    /// capture policy returns [`CaptureDecision::Backoff`] so a slow client
    /// slows the camera instead of exhausting the heap.
    pub fn note_write_stalled(&mut self, now_ms: u64) {
        self.cooldown_until_ms = now_ms.saturating_add(self.cooldown_ms);
    }

    /// Feed an event; returns the effects to apply, in order.
    ///
    /// Transition table (unit-tested):
    /// - `Idle + ClientConnected` → `Active` `[StartCapture]`
    /// - `Active + ClientDisconnected` → `TornDown` `[StopCapture, TearDownServer]` (d)
    /// - `Active + WifiLost` → `TornDown` `[StopCapture, TearDownServer]`
    /// - `TornDown + ClientConnected` → `Active` `[StartCapture]` (device retries)
    /// - `TornDown + WifiRestored` → `Idle` `[]` (ready for next client)
    /// - every other pair is a no-op (`[]`); duplicate events never double-fire
    pub fn handle(&mut self, event: Event) -> Vec<SessionAction> {
        use Event::*;
        use SessionAction::*;
        use SessionState::*;

        match (self.state, event) {
            (Idle | TornDown, ClientConnected) => {
                self.state = Active;
                vec![StartCapture]
            }
            (Active, ClientDisconnected) => {
                self.state = TornDown;
                vec![StopCapture, TearDownServer]
            }
            (Active, WifiLost) => {
                self.state = TornDown;
                vec![StopCapture, TearDownServer]
            }
            (TornDown, WifiRestored) => {
                self.state = Idle;
                vec![]
            }
            _ => vec![],
        }
    }

    /// The per-tick capture policy (behaviours (b) + (c)).
    ///
    /// `dma_free` / `dma_largest` come from
    /// `heap_caps_get_free_size(MALLOC_CAP_DMA)` /
    /// `heap_caps_get_largest_free_block(MALLOC_CAP_DMA)` on the device;
    /// tests pass literal boundary values.
    pub fn decide(&self, dma_free: usize, dma_largest: usize, now_ms: u64) -> CaptureDecision {
        if self.state != SessionState::Active {
            return CaptureDecision::Suspend;
        }
        if now_ms < self.cooldown_until_ms {
            return CaptureDecision::Backoff(BackoffReason::Cooldown {
                until_ms: self.cooldown_until_ms,
            });
        }
        if dma_free < DMA_FREE_MIN_BYTES {
            return CaptureDecision::Backoff(BackoffReason::DmaFreeLow { free: dma_free });
        }
        if dma_largest < DMA_LARGEST_MIN_BYTES {
            return CaptureDecision::Backoff(BackoffReason::DmaLargestLow {
                largest: dma_largest,
            });
        }
        CaptureDecision::Capture
    }
}

/// Module-level session shared by `net.rs` (supervisor tick + Wi-Fi events)
/// and, from Todo 11 on, the RTSP layer's disconnect callback.
pub static SESSION: Mutex<Session> = Mutex::new(Session::with_defaults());

/// Poison-tolerant lock helper (a panicked holder must not wedge the
/// supervisor thread; the policy has no unwrappable invariants).
pub fn with_session<F, R>(f: F) -> R
where
    F: FnOnce(&mut Session) -> R,
{
    let mut guard = SESSION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KB: usize = 1024;

    #[test]
    fn dma_free_boundary_is_strictly_below_12k() {
        let mut s = Session::with_defaults();
        s.handle(Event::ClientConnected);
        // Exactly 12 KiB free → capture (plan: "below 12 KB" backs off).
        assert_eq!(
            s.decide(DMA_FREE_MIN_BYTES, 64 * KB, 0),
            CaptureDecision::Capture
        );
        // One byte below → back off with the free-size reason.
        assert_eq!(
            s.decide(DMA_FREE_MIN_BYTES - 1, 64 * KB, 0),
            CaptureDecision::Backoff(BackoffReason::DmaFreeLow {
                free: DMA_FREE_MIN_BYTES - 1
            })
        );
    }

    #[test]
    fn dma_largest_boundary_is_strictly_below_4k() {
        let mut s = Session::with_defaults();
        s.handle(Event::ClientConnected);
        assert_eq!(
            s.decide(64 * KB, DMA_LARGEST_MIN_BYTES, 0),
            CaptureDecision::Capture
        );
        assert_eq!(
            s.decide(64 * KB, DMA_LARGEST_MIN_BYTES - 1, 0),
            CaptureDecision::Backoff(BackoffReason::DmaLargestLow {
                largest: DMA_LARGEST_MIN_BYTES - 1
            })
        );
    }

    #[test]
    fn dma_free_takes_precedence_over_largest_when_both_low() {
        let mut s = Session::with_defaults();
        s.handle(Event::ClientConnected);
        assert_eq!(
            s.decide(DMA_FREE_MIN_BYTES - 1, 1, 0),
            CaptureDecision::Backoff(BackoffReason::DmaFreeLow {
                free: DMA_FREE_MIN_BYTES - 1
            })
        );
    }

    #[test]
    fn no_client_suspends_capture_even_with_healthy_dma() {
        let s = Session::with_defaults();
        assert_eq!(s.state(), SessionState::Idle);
        assert_eq!(
            s.decide(1024 * KB, 256 * KB, 0),
            CaptureDecision::Suspend
        );
    }

    #[test]
    fn slow_client_cooldown_slows_camera_then_recovers() {
        let mut s = Session::new(500);
        s.handle(Event::ClientConnected);
        assert_eq!(
            s.decide(64 * KB, 64 * KB, 1_000),
            CaptureDecision::Capture
        );
        // Behaviour (c): a write stall arms the cooldown.
        s.note_write_stalled(1_000);
        assert_eq!(s.cooldown_until_ms(), 1_500);
        assert_eq!(
            s.decide(64 * KB, 64 * KB, 1_499),
            CaptureDecision::Backoff(BackoffReason::Cooldown { until_ms: 1_500 })
        );
        // Frame is dropped during the window (never queued), then capture resumes.
        assert_eq!(
            s.decide(64 * KB, 64 * KB, 1_500),
            CaptureDecision::Capture
        );
    }

    #[test]
    fn disconnect_tears_down_from_the_callback() {
        let mut s = Session::with_defaults();
        assert_eq!(s.handle(Event::ClientConnected), vec![SessionAction::StartCapture]);
        assert_eq!(s.state(), SessionState::Active);
        // Behaviour (d): the actions are returned *by handling the disconnect
        // event itself* — the caller applies TearDownServer inline.
        assert_eq!(
            s.handle(Event::ClientDisconnected),
            vec![SessionAction::StopCapture, SessionAction::TearDownServer]
        );
        assert_eq!(s.state(), SessionState::TornDown);
        // Capture is suspended after teardown.
        assert_eq!(
            s.decide(64 * KB, 64 * KB, 0),
            CaptureDecision::Suspend
        );
        // Duplicate disconnect is a no-op (no double teardown).
        assert_eq!(s.handle(Event::ClientDisconnected), Vec::<SessionAction>::new());
    }

    #[test]
    fn wifi_loss_tears_down_and_restore_rearms() {
        let mut s = Session::with_defaults();
        s.handle(Event::ClientConnected);
        assert_eq!(
            s.handle(Event::WifiLost),
            vec![SessionAction::StopCapture, SessionAction::TearDownServer]
        );
        // AP kill while idle: no teardown spam.
        let mut idle = Session::with_defaults();
        assert_eq!(idle.handle(Event::WifiLost), Vec::<SessionAction>::new());
        // Restore prepares the device for the retrying client.
        assert_eq!(s.handle(Event::WifiRestored), Vec::<SessionAction>::new());
        assert_eq!(s.state(), SessionState::Idle);
        assert_eq!(
            s.handle(Event::ClientConnected),
            vec![SessionAction::StartCapture]
        );
    }

    #[test]
    fn second_client_while_active_is_ignored_by_session() {
        // Todo 11 refuses the second client itself; the session must not
        // double-start capture or corrupt state.
        let mut s = Session::with_defaults();
        s.handle(Event::ClientConnected);
        assert_eq!(s.handle(Event::ClientConnected), Vec::<SessionAction>::new());
        assert_eq!(s.state(), SessionState::Active);
    }

    #[test]
    fn cooldown_is_server_configurable() {
        let mut s = Session::new(2_000);
        assert_eq!(s.cooldown_ms(), 2_000);
        s.handle(Event::ClientConnected);
        s.note_write_stalled(10);
        assert_eq!(s.cooldown_until_ms(), 2_010);
        s.set_cooldown_ms(100);
        assert_eq!(s.cooldown_ms(), 100);
    }

    #[test]
    fn with_session_helper_runs_the_policy() {
        // Uses the module-level static; restores it so other tests (plan
        // fact 3: no shared mutable state between tests) are unaffected.
        // Isolation (task 21): reset whatever another test in this binary
        // left in SESSION (the rtsp::service loopback ends non-Idle), so
        // the entry state is deterministic under any order/thread count.
        with_session(|s| {
            *s = Session::with_defaults();
            let before = s.state();
            let _ = s.handle(Event::ClientConnected);
            assert_eq!(s.state(), SessionState::Active);
            let _ = s.handle(Event::ClientDisconnected);
            assert_eq!(s.state(), SessionState::TornDown);
            let _ = s.handle(Event::WifiRestored);
            assert_eq!(s.state(), SessionState::Idle);
            assert_eq!(before, SessionState::Idle);
        });
    }
}
