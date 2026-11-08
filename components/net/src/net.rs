//! Wi-Fi STA, mDNS discovery, backpressure supervision and SNTP (plan Todo 14).
//!
//! Plan contract, quoted from `.omo/plans/esp32s3-av-stream-and-sd-recording.md`
//! Todo 14 "References" / "Do" / "Also establish the time source":
//!
//! > `esp_idf_svc::wifi` for STA; `esp_idf_svc::mdns` for `_rtsp._tcp`
//! > advertisement. [...] (a) **disable Wi-Fi power save** before streaming —
//! > the reference logs *"RTSP streaming may hit TX backpressure"* if this is
//! > skipped; [...] Advertise over mDNS so a consumer can find the board
//! > without knowing its IP. Log the assigned IP at Info. Implement the four
//! > behaviours above.
//! >
//! > Initialise SNTP via `esp_idf_svc::sntp` once the interface is up:
//! > `pool.ntp.org` with an NTP fallback pool, UTC timezone (SNTP is UTC-only;
//! > applying a local offset afterwards is a bug), and a 60-minute resync
//! > interval. Expose `sntp::last_sync_utc() -> Option<SystemTime>` and a
//! > `is_synced()` flag. If SNTP fails, the flag stays false, the failure is
//! > logged once with the reason, and Todo 13 suppresses Sender Reports rather
//! > than fabricating a wallclock.
//!
//! **`EspSntp` handle held for process lifetime — micro-rdk lesson.** LWIP's
//! SNTP client is stopped by `EspSntp::drop` (`sntp_stop()`), so a handle that
//! goes out of scope produces a device that synced once and then silently
//! stops — which fails much later and much more confusingly than a device that
//! never synced. This module therefore stores the handle in a module-level
//! `static SNTP_HANDLE` that no caller can drop; `bring_up` only ever replaces
//! it (fallback-server swap, which goes through `Drop` → re-init deliberately).
//! Todo 13 must read time via [`is_synced`] / [`last_sync_utc`], never via a
//! handle it owns.
//!
//! Behaviours (b)/(c)/(d) live in [`crate::session`] (pure policy); this file
//! is the device glue that samples the heap, feeds events, applies returned
//! actions, and logs the exact lines quoted in
//! `.omo/evidence/task-14/serial.log`.
//!
//! **Credentials (IS-6).** [`WifiCredentials`] is a runtime-injected config
//! struct; this file contains no SSID/password literal of any kind. The
//! `EXAMPLE_SSSID`-style placeholders used in tests are fake by construction;
//! real credentials are supplied by the boot wiring (Todo 20) from a channel
//! that is never committed to the repository.
//!
//! The pure layer ([`NetConfig`], [`SntpClock`], backoff maths) has no
//! `esp_idf_*` imports so it runs as host-executed tests via
//! `harness/tests/task14_net.rs`; the device glue is `cfg(target_os =
//! "espidf")`-gated.

use std::sync::Mutex;
use std::time::SystemTime;

// ---------------------------------------------------------------------------
// Pure configuration (host-compilable)
// ---------------------------------------------------------------------------

/// Runtime-injected Wi-Fi credentials. Never hardcoded (IS-6); the fields are
/// plain `String`s so the type itself carries no default "helpful" value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WifiCredentials {
    pub ssid: String,
    pub password: String,
}

impl WifiCredentials {
    pub fn new(ssid: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            ssid: ssid.into(),
            password: password.into(),
        }
    }

    /// Validate against the WPA2-PSK limits the driver itself enforces, so a
    /// bad config fails as a typed error here instead of a 15 s connect
    /// timeout on the device.
    ///
    /// - SSID: 1..=32 bytes (IEEE 802.11 SSID length).
    /// - Passphrase: 8..=63 ASCII bytes, or exactly 64 hex digits (raw PSK).
    pub fn validate(&self) -> Result<(), NetError> {
        let ssid_len = self.ssid.len();
        if !(1..=32).contains(&ssid_len) {
            return Err(NetError::SsidInvalid { len: ssid_len });
        }
        let pw_len = self.password.len();
        let passphrase = (8..=63).contains(&pw_len);
        let raw_psk = pw_len == 64 && self.password.chars().all(|c| c.is_ascii_hexdigit());
        if !passphrase && !raw_psk {
            return Err(NetError::PasswordInvalid { len: pw_len });
        }
        Ok(())
    }
}

/// Full network configuration, injected at runtime by the boot path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetConfig {
    pub credentials: WifiCredentials,
    /// mDNS hostname (device appears as `<hostname>.local`).
    pub hostname: String,
    /// mDNS instance label.
    pub instance_name: String,
    /// RTSP port advertised as `_rtsp._tcp` (554 per plan / Todo 11).
    pub rtsp_port: u16,
    /// Primary NTP server (plan default `pool.ntp.org`).
    pub sntp_primary: String,
    /// Fallback NTP pool, used when the primary has not synced in
    /// [`FALLBACK_AFTER_MS`] (plan: "an NTP fallback pool" so the A/V gate is
    /// usable air-gapped).
    pub sntp_fallback: String,
    /// Slow-client cooldown handed to `session::Session` (plan behaviour (c),
    /// "server-provided cooldown").
    pub cooldown_ms: u64,
}

impl NetConfig {
    /// Plan defaults with **fake** credentials — usable for host tests and as
    /// a compile-time template; the boot path supplies real values.
    pub fn with_defaults() -> Self {
        Self {
            credentials: WifiCredentials::default(),
            hostname: "esp32s3-av".to_string(),
            instance_name: "esp32s3-av RTSP".to_string(),
            rtsp_port: 554,
            sntp_primary: "pool.ntp.org".to_string(),
            sntp_fallback: "time.cloudflare.com".to_string(),
            cooldown_ms: crate::session::DEFAULT_COOLDOWN_MS,
        }
    }

    pub fn validate(&self) -> Result<(), NetError> {
        self.credentials.validate()?;
        if self.hostname.is_empty() || self.hostname.len() > 63 {
            return Err(NetError::HostnameInvalid);
        }
        if self.rtsp_port == 0 {
            return Err(NetError::ConfigInvalid("rtsp_port must be non-zero"));
        }
        if self.sntp_primary.is_empty() {
            return Err(NetError::ConfigInvalid("sntp_primary is empty"));
        }
        Ok(())
    }
}

/// Typed errors for network bring-up (house style: Display + Error, no
/// string-matching at call sites).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetError {
    SsidInvalid { len: usize },
    PasswordInvalid { len: usize },
    HostnameInvalid,
    ConfigInvalid(&'static str),
    EventLoopTaken,
    NvsTaken,
    WifiInit,
    WifiConfig,
    WifiStart,
    WifiConnect,
    WifiNetifUp,
    MdnsInit,
    MdnsConfig,
    ThreadSpawn,
}

impl std::fmt::Display for NetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SsidInvalid { len } => write!(f, "SSID length {len} outside 1..=32 bytes"),
            Self::PasswordInvalid { len } => {
                write!(f, "password length {len} is neither a 8..=63 passphrase nor a 64-hex PSK")
            }
            Self::HostnameInvalid => write!(f, "hostname empty or longer than 63 bytes"),
            Self::ConfigInvalid(what) => write!(f, "invalid network config: {what}"),
            Self::EventLoopTaken => write!(f, "system event loop already taken"),
            Self::NvsTaken => write!(f, "default NVS partition already taken"),
            Self::WifiInit => write!(f, "EspWifi::new failed"),
            Self::WifiConfig => write!(f, "set_configuration failed"),
            Self::WifiStart => write!(f, "Wi-Fi driver start failed"),
            Self::WifiConnect => write!(f, "Wi-Fi connect failed or timed out (15 s)"),
            Self::WifiNetifUp => write!(f, "station netif did not come up (15 s)"),
            Self::MdnsInit => write!(f, "EspMdns::take failed"),
            Self::MdnsConfig => write!(f, "mDNS hostname/service registration failed"),
            Self::ThreadSpawn => write!(f, "supervisor thread spawn failed"),
        }
    }
}

impl std::error::Error for NetError {}

/// Wi-Fi reconnect backoff after a link loss: 1 s, 2 s, 4 s, 8 s, 16 s, then
/// capped at 30 s ([`RETRY_CAP_MS`]). Pure so the growth curve is
/// host-tested; the supervisor applies it.
pub const RETRY_BASE_MS: u64 = 1_000;
pub const RETRY_CAP_MS: u64 = 30_000;

pub fn retry_backoff_ms(attempt: u32) -> u64 {
    let shift = attempt.saturating_sub(1).min(5);
    (RETRY_BASE_MS << shift).min(RETRY_CAP_MS)
}

/// How long after SNTP init we log the one-time "not synced" failure line
/// (plan: "the failure is logged once with the reason"). 90 s = startup delay
/// (CONFIG_LWIP_SNTP_STARTUP_DELAY ≤ 5 s) plus several poll attempts.
pub const FAILURE_LOG_AFTER_MS: u64 = 90_000;

/// How long after SNTP init we swap to [`NetConfig::sntp_fallback`].
/// `SNTP_MAX_SERVERS = 1` in this build (verified: `CONFIG_LWIP_SNTP_MAX_SERVERS=1`),
/// so the "NTP fallback pool" is realised by re-initialising `EspSntp` with
/// the fallback name — legal because `EspSntp::drop` clears the module's
/// `TAKEN` guard before `sntp_init` runs again.
pub const FALLBACK_AFTER_MS: u64 = 120_000;

/// SNTP resync interval actually in force: LWIP polls every
/// `CONFIG_LWIP_SNTP_UPDATE_DELAY = 3600000` ms = 60 min, which is exactly
/// the plan's "60-minute resync" — verified in the generated sdkconfig, no
/// sdkconfig change required.
pub const RESYNC_INTERVAL_SECS: u64 = 3600;

// ---------------------------------------------------------------------------
// Pure SNTP clock state (host-compilable; Todo 13 consumes this)
// ---------------------------------------------------------------------------

/// Why SNTP was declared failed (the "reason" the plan requires in the
/// single failure log line).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SntpFailureReason {
    /// No successful sync within [`FAILURE_LOG_AFTER_MS`] of init — server
    /// unreachable or DNS failed (LWIP exposes no finer-grained reason for a
    /// polled sync).
    NoResponseWithinTimeout,
}

impl std::fmt::Display for SntpFailureReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoResponseWithinTimeout => f.write_str(
                "no successful sync within 90 s of init (NTP server unreachable or DNS failed)",
            ),
        }
    }
}

/// Wallclock state derived from SNTP. `synced` means "has synced at least
/// once" — LWIP keeps the system clock after that, so RTCP Sender Reports
/// may keep using it (a stale-but-real wallclock is strictly better than a
/// synthesised one; a never-synced clock must emit nothing at all).
pub struct SntpClock {
    last_sync: Option<SystemTime>,
    started_at_ms: Option<u64>,
    failure_logged: bool,
    fallback_armed: bool,
}

impl SntpClock {
    pub const fn new() -> Self {
        Self {
            last_sync: None,
            started_at_ms: None,
            failure_logged: false,
            fallback_armed: false,
        }
    }

    /// Called by the supervisor once `EspSntp` is initialised (monotonic ms).
    pub fn set_started(&mut self, now_ms: u64) {
        if self.started_at_ms.is_none() {
            self.started_at_ms = Some(now_ms);
        }
    }

    /// Force the one-shot failure flag without waiting for the timeout —
    /// used when `EspSntp::new` itself fails at bring-up, so the 90 s tick
    /// cannot emit a second "not synced" line (plan: logged **once**).
    pub fn mark_failure_logged(&mut self) {
        self.failure_logged = true;
    }

    pub fn record_sync(&mut self, at: SystemTime) {
        self.last_sync = Some(at);
    }

    pub fn is_synced(&self) -> bool {
        self.last_sync.is_some()
    }

    /// The plan's `sntp::last_sync_utc() -> Option<SystemTime>`.
    pub fn last_sync_utc(&self) -> Option<SystemTime> {
        self.last_sync
    }

    /// The suppression predicate Todo 13 consumes: RTCP Sender Reports are
    /// emitted **only** when a real NTP wallclock exists. Never synthesise.
    pub fn may_send_rtcp(&self) -> bool {
        self.is_synced()
    }

    /// One-shot: returns `Some(reason)` exactly once, when the unsynced
    /// timeout elapses and nothing has synced yet.
    pub fn take_failure_report(&mut self, now_ms: u64) -> Option<SntpFailureReason> {
        if self.last_sync.is_some() || self.failure_logged {
            return None;
        }
        let started = self.started_at_ms?;
        if now_ms.saturating_sub(started) >= FAILURE_LOG_AFTER_MS {
            self.failure_logged = true;
            Some(SntpFailureReason::NoResponseWithinTimeout)
        } else {
            None
        }
    }

    /// One-shot: returns `true` exactly once when the fallback swap should run.
    pub fn arm_fallback_once(&mut self, now_ms: u64) -> bool {
        if self.last_sync.is_some() || self.fallback_armed {
            return false;
        }
        let started = match self.started_at_ms {
            Some(s) => s,
            None => return false,
        };
        if now_ms.saturating_sub(started) >= FALLBACK_AFTER_MS {
            self.fallback_armed = true;
            true
        } else {
            false
        }
    }

    pub fn fallback_armed(&self) -> bool {
        self.fallback_armed
    }
}

/// Module-level clock; `is_synced()` / `last_sync_utc()` read it without
/// needing the `EspSntp` handle.
static SNTP_CLOCK: Mutex<SntpClock> = Mutex::new(SntpClock::new());

pub fn with_sntp_clock<F, R>(f: F) -> R
where
    F: FnOnce(&mut SntpClock) -> R,
{
    let mut guard = SNTP_CLOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut guard)
}

/// Plan-exposed: `is_synced()` flag (Todo 13 gate).
pub fn is_synced() -> bool {
    with_sntp_clock(|c| c.is_synced())
}

/// Plan-exposed: `sntp::last_sync_utc() -> Option<SystemTime>`.
pub fn last_sync_utc() -> Option<SystemTime> {
    with_sntp_clock(|c| c.last_sync_utc())
}

/// Todo 13's suppression predicate: emit an RTCP Sender Report only when a
/// real SNTP wallclock exists (plan Todo 13 QA — failure: "assert that
/// Sender Reports are suppressed entirely and the log says 'unsynced, SR
/// suppressed'").
pub fn may_send_rtcp() -> bool {
    with_sntp_clock(|c| c.may_send_rtcp())
}

// ---------------------------------------------------------------------------
// Device glue
// ---------------------------------------------------------------------------

#[cfg(target_os = "espidf")]
mod esp_impl {
    use std::sync::Mutex;
    use std::thread;
    use std::time::{Duration, SystemTime};

    use esp_idf_svc::eventloop::EspSystemEventLoop;
    use esp_idf_svc::hal::modem::Modem;
    use esp_idf_svc::mdns::EspMdns;
    use esp_idf_svc::nvs::EspDefaultNvsPartition;
    use esp_idf_svc::sntp::{EspSntp, OperatingMode, SntpConf, SyncMode, SyncStatus};
    use esp_idf_svc::sys::EspError;
    use esp_idf_svc::wifi::{BlockingWifi, Configuration, EspWifi};

    use crate::session::{self, Event, SessionAction};

    use super::{
        is_synced, retry_backoff_ms, with_sntp_clock, NetConfig, NetError, RESYNC_INTERVAL_SECS,
    };

    /// Private FFI boundary — the ONLY `unsafe` in this file (house rule:
    /// every executable `unsafe` lives inside `mod ffi`, same discipline as
    /// `camera.rs` / `storage.rs`).
    mod ffi {
        use esp_idf_svc::sys::{
            esp_log_level_t_ESP_LOG_INFO as ESP_LOG_INFO,
            esp_log_write, esp_netif_init, esp_timer_get_time, esp_wifi_set_ps,
            heap_caps_get_free_size, heap_caps_get_largest_free_block,
            wifi_ps_type_t_WIFI_PS_NONE, EspError,
        };
        use std::ffi::CString;

        /// Bring up the LwIP TCP/IP stack and the default event loop.
        ///
        /// `esp_netif_init()` runs `tcpip_init()`. The system event loop is
        /// created separately by `EspSystemEventLoop::take()` in the Wi-Fi
        /// bring-up path.
        pub fn netif_init() -> Result<(), EspError> {
            EspError::convert(unsafe { esp_netif_init() })
        }

        /// Plan (a): disable Wi-Fi power save before streaming.
        pub fn wifi_disable_power_save() -> Result<(), EspError> {
            let rc = unsafe { esp_wifi_set_ps(wifi_ps_type_t_WIFI_PS_NONE) };
            EspError::convert(rc)
        }

        /// Monotonic milliseconds (esp_timer, same clock the mic/recorder use).
        pub fn now_ms() -> u64 {
            let us = unsafe { esp_timer_get_time() };
            (us.max(0) as u64) / 1000
        }

        /// Behaviour (b) probe: DMA-capable heap free + largest block.
        pub fn dma_sample() -> (usize, usize) {
            const MALLOC_CAP_DMA: u32 = 8;
            unsafe {
                (
                    heap_caps_get_free_size(MALLOC_CAP_DMA),
                    heap_caps_get_largest_free_block(MALLOC_CAP_DMA),
                )
            }
        }

        /// Literal ESP-IDF Info-level line: serial shows `I (...) net: ...`.
        /// `EspError::convert`/variadic call are confined here.
        pub fn log_info(tag: &str, msg: &str) {
            log_at(ESP_LOG_INFO, tag, msg);
        }

        fn log_at(level: esp_idf_svc::sys::esp_log_level_t, tag: &str, msg: &str) {
            let tag = CString::new(tag).unwrap_or_default();
            let msg = CString::new(msg).unwrap_or_default();
            unsafe {
                esp_log_write(
                    level,
                    tag.as_ptr(),
                    b"%s\0".as_ptr() as *const _,
                    msg.as_ptr(),
                );
            }
        }
    }

    /// What `bring_up` hands back to the boot path (Todo 20). **Hold this for
    /// the process lifetime**: dropping it drops the mDNS advertiser. The
    /// Wi-Fi supervisor (and with it the Wi-Fi driver, which it owns) runs on
    /// its own thread and is not stopped by dropping this handle — the join
    /// handle only detaches.
    pub struct NetHandles {
        _mdns: EspMdns,
        _supervisor: thread::JoinHandle<()>,
    }

    /// Bring up LwIP so any later socket call has a live TCP/IP mbox. The
    /// Todo 20 boot path calls this before `bring_up`; on-device tests that
    /// bind or connect a real socket call it first. Repeat calls are `ESP_OK`,
    /// so it needs no guard.
    pub fn ensure_netif_initialised() -> Result<(), EspError> {
        ffi::netif_init()
    }

    static SNTP_HANDLE: Mutex<Option<EspSntp<'static>>> = Mutex::new(None);

    fn sntp_lock<F, R>(f: F) -> R
    where
        F: FnOnce(&mut Option<EspSntp<'static>>) -> R,
    {
        let mut guard = SNTP_HANDLE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut guard)
    }

    fn sync_callback(d: Duration) {
        if let Some(at) = SystemTime::UNIX_EPOCH.checked_add(d) {
            with_sntp_clock(|c| c.record_sync(at));
            ffi::log_info("net", &format!("sntp: synced at {at:?} (is_synced=true)"));
        }
    }

    fn start_sntp(server: &str) -> Result<EspSntp<'static>, EspError> {
        let mut conf = SntpConf::default();
        // `SNTP_MAX_SERVERS` is 1 in this sdkconfig; fill every slot so the
        // code stays correct if the limit is ever raised.
        for slot in conf.servers.iter_mut() {
            *slot = server;
        }
        conf.operating_mode = OperatingMode::Poll;
        conf.sync_mode = SyncMode::Immediate;
        EspSntp::new_with_callback(&conf, sync_callback)
    }

    /// Bring up STA + mDNS + SNTP and start the
    /// supervisor thread. The initial connect is attempted here but **its
    /// failure is not fatal**: the supervisor keeps retrying with backoff
    /// (plan acceptance: "disconnecting Wi-Fi tears the session down cleanly
    /// and the device retries").
    pub fn bring_up(modem: Modem<'static>, config: &NetConfig) -> Result<NetHandles, NetError> {
        config.validate()?;

        // Plan: "UTC timezone (SNTP is UTC-only; applying a local offset
        // afterwards is a bug)" — pin TZ so every later conversion is UTC.
        std::env::set_var("TZ", "UTC0");

        let sys_loop =
            EspSystemEventLoop::take().map_err(|_| NetError::EventLoopTaken)?;
        let nvs = EspDefaultNvsPartition::take().map_err(|_| NetError::NvsTaken)?;

        let esp_wifi =
            EspWifi::new(modem, sys_loop.clone(), Some(nvs)).map_err(|_| NetError::WifiInit)?;
        let mut wifi = BlockingWifi::wrap(esp_wifi, sys_loop).map_err(|_| NetError::WifiInit)?;

        let credentials = &config.credentials;
        let client = esp_idf_svc::wifi::ClientConfiguration {
            ssid: credentials
                .ssid
                .as_str()
                .try_into()
                .map_err(|_| NetError::SsidInvalid {
                    len: credentials.ssid.len(),
                })?,
            password: credentials
                .password
                .as_str()
                .try_into()
                .map_err(|_| NetError::PasswordInvalid {
                    len: credentials.password.len(),
                })?,
            ..Default::default()
        };
        wifi.set_configuration(&Configuration::Client(client))
            .map_err(|e| {
                ffi::log_info("net", &format!("set_configuration failed: {e}"));
                NetError::WifiConfig
            })?;

        wifi.start().map_err(|e| {
            ffi::log_info("net", &format!("Wi-Fi start failed: {e}"));
            NetError::WifiStart
        })?;

        // Plan behaviour (a) — FIRST thing after the driver is up, before any
        // streaming: without this, "RTSP streaming may hit TX backpressure".
        ffi::wifi_disable_power_save().map_err(|e| {
            ffi::log_info(
                "net",
                &format!("esp_wifi_set_ps(WIFI_PS_NONE) failed: {e}"),
            );
            NetError::WifiStart
        })?;
        ffi::log_info(
            "net",
            "Wi-Fi power save disabled (esp_wifi_set_ps=WIFI_PS_NONE) — plan (a): \"RTSP streaming may hit TX backpressure\" if skipped",
        );

        // Initial attempt: bounded (15 s connect + 15 s netif). Failure
        // falls through to the supervisor's retry loop.
        let mut was_connected = false;
        match wifi.connect().and_then(|()| wifi.wait_netif_up()) {
            Ok(()) => {
                was_connected = true;
                log_ip("net", &wifi, 0);
            }
            Err(e) => {
                ffi::log_info(
                    "net",
                    &format!(
                        "initial Wi-Fi connect not ready ({e}) — supervisor retries with backoff"
                    ),
                );
            }
        }

        // mDNS: advertise the RTSP service so a consumer can find the board
        // without knowing its IP (plan Do).
        let mut mdns = EspMdns::take().map_err(|_| NetError::MdnsInit)?;
        mdns.set_hostname(config.hostname.as_str())
            .map_err(|_| NetError::MdnsConfig)?;
        mdns.set_instance_name(config.instance_name.as_str())
            .map_err(|_| NetError::MdnsConfig)?;
        mdns.add_service(
            Some(config.instance_name.as_str()),
            "_rtsp",
            "_tcp",
            config.rtsp_port,
            &[("path", "/mjpeg/1")],
        )
        .map_err(|_| NetError::MdnsConfig)?;
        ffi::log_info(
            "net",
            &format!(
                "mdns: hostname={}.local service=_rtsp._tcp port={} txt=path:/mjpeg/1",
                config.hostname, config.rtsp_port
            ),
        );

        // SNTP — once the interface is up (plan). Handle goes into a static;
        // it can never be dropped by a caller.
        with_sntp_clock(|c| c.set_started(ffi::now_ms()));
        match start_sntp(&config.sntp_primary) {
            Ok(handle) => {
                sntp_lock(|slot| *slot = Some(handle));
                ffi::log_info(
                    "net",
                    &format!(
                        "sntp: initialized server={} fallback={} resync={}s timezone=UTC0 (TZ=UTC0)",
                        config.sntp_primary, config.sntp_fallback, RESYNC_INTERVAL_SECS
                    ),
                );
            }
            Err(e) => {
                // Failure path: is_synced() stays false, one log line, no panic.
                with_sntp_clock(|c| c.mark_failure_logged());
                ffi::log_info(
                    "net",
                    &format!(
                        "sntp: init failed ({e}); reason: SNTP service unavailable — is_synced=false, RTCP SRs suppressed"
                    ),
                );
            }
        }

        // Session cooldown (plan (c): server-provided).
        session::with_session(|s| s.set_cooldown_ms(config.cooldown_ms));

        let supervisor_cfg = config.clone();
        let supervisor = thread::Builder::new()
            .name("net-supervisor".into())
            .stack_size(16 * 1024)
            .spawn(move || supervisor_loop(wifi, supervisor_cfg, was_connected))
            .map_err(|_| NetError::ThreadSpawn)?;

        Ok(NetHandles {
            _mdns: mdns,
            _supervisor: supervisor,
        })
    }

    fn log_ip(tag: &str, wifi: &BlockingWifi<EspWifi<'static>>, attempt: u32) {
        match wifi.wifi().sta_netif().get_ip_info() {
            Ok(ip) => ffi::log_info(
                tag,
                &format!(
                    "INFO ip addr={} gw={} netmask={} (attempt={attempt})",
                    ip.ip, ip.subnet.gateway, ip.subnet.mask
                ),
            ),
            Err(e) => ffi::log_info(tag, &format!("ip addr unavailable: {e}")),
        }
    }

    /// Apply the actions a session transition demands, logging each (plan
    /// behaviour (d): teardown is driven by the disconnect event itself).
    fn apply_session_actions(actions: &[SessionAction], reason: &str) {
        for action in actions {
            match action {
                SessionAction::StartCapture => {
                    ffi::log_info("session", "capture started (state=Active)")
                }
                SessionAction::StopCapture => {
                    ffi::log_info("session", &format!("capture stopped (reason={reason})"))
                }
                SessionAction::TearDownServer => ffi::log_info(
                    "session",
                    &format!("teardown from disconnect callback (reason={reason})"),
                ),
            }
        }
    }

    fn supervisor_loop(
        mut wifi: BlockingWifi<EspWifi<'static>>,
        config: NetConfig,
        mut was_connected: bool,
    ) {
        let mut attempt: u32 = 0;
        let mut next_retry_ms: u64 = 0;
        let mut last_decision: Option<session::CaptureDecision> = None;

        loop {
            thread::sleep(Duration::from_millis(500));
            let now = ffi::now_ms();

            let connected = wifi.is_connected().unwrap_or(false);
            if connected && !was_connected {
                let failed_attempts = attempt;
                attempt = 0;
                log_ip("net", &wifi, failed_attempts);
                let actions = session::with_session(|s| s.handle(Event::WifiRestored));
                apply_session_actions(&actions, "wifi-restored");
                ffi::log_info(
                    "net",
                    &format!("Wi-Fi link up after {failed_attempts} failed attempt(s)"),
                );
                was_connected = true;
            } else if !connected && was_connected {
                // Plan QA — failure: "kill the AP mid-stream and assert the
                // server tears down, the log says so, and reconnection is
                // attempted."
                ffi::log_info(
                    "net",
                    "WARN Wi-Fi link lost — tearing down session (plan QA: AP kill)",
                );
                let actions = session::with_session(|s| s.handle(Event::WifiLost));
                apply_session_actions(&actions, "wifi-lost");
                was_connected = false;
                attempt = 1;
                next_retry_ms = now + retry_backoff_ms(attempt);
            }

            if !connected && now >= next_retry_ms {
                attempt = attempt.saturating_add(1);
                let backoff = retry_backoff_ms(attempt);
                ffi::log_info(
                    "net",
                    &format!(
                        "Wi-Fi reconnect attempt {attempt} (next backoff {backoff} ms)"
                    ),
                );
                match wifi.connect() {
                    Ok(()) => match wifi.wait_netif_up() {
                        Ok(()) => {}
                        Err(e) => ffi::log_info(
                            "net",
                            &format!("reconnect: netif up wait failed: {e}"),
                        ),
                    },
                    Err(e) => ffi::log_info("net", &format!("reconnect attempt failed: {e}")),
                }
                next_retry_ms = ffi::now_ms() + retry_backoff_ms(attempt + 1);
            }

            // Behaviour (b)/(c) tick: sample DMA, decide, log CHANGES only.
            let (dma_free, dma_largest) = ffi::dma_sample();
            let decision =
                session::with_session(|s| s.decide(dma_free, dma_largest, ffi::now_ms()));
            if last_decision != Some(decision) {
                log_decision(decision);
                last_decision = Some(decision);
            }

            // SNTP supervision: one failure line + one fallback swap, both
            // once (plan: "the failure is logged once with the reason").
            if let Some(reason) = with_sntp_clock(|c| c.take_failure_report(now)) {
                ffi::log_info(
                    "net",
                    &format!(
                        "WARN sntp: not synced ({reason}); is_synced=false, RTCP Sender Reports stay suppressed"
                    ),
                );
            }
            if with_sntp_clock(|c| c.arm_fallback_once(now)) {
                let server = config.sntp_fallback.clone();
                ffi::log_info(
                    "net",
                    &format!("sntp: switching to fallback server {server} (primary did not sync)"),
                );
                // Order matters: the old handle must be DROPPED first —
                // `EspSntp::drop` runs `sntp_stop()` and clears the TAKEN
                // guard, without which the re-init returns INVALID_STATE.
                sntp_lock(|slot| {
                    *slot = None;
                    match start_sntp(&server) {
                        Ok(handle) => *slot = Some(handle),
                        Err(e) => ffi::log_info(
                            "net",
                            &format!("sntp: fallback init failed ({e}); is_synced stays false"),
                        ),
                    }
                });
            }

            // Belt-and-braces: if LWIP reports a completed sync but the
            // callback somehow never fired, adopt the now-correct system
            // clock once (it is only correct *because* SNTP completed).
            if !is_synced() {
                let completed = sntp_lock(|slot| {
                    slot.as_ref()
                        .map(|h| h.get_sync_status() == SyncStatus::Completed)
                        .unwrap_or(false)
                });
                if completed {
                    with_sntp_clock(|c| c.record_sync(SystemTime::now()));
                    ffi::log_info("net", "sntp: synced (status Completed, callback missed)");
                }
            }
        }
    }

    fn log_decision(decision: session::CaptureDecision) {
        use session::{BackoffReason, CaptureDecision};
        match decision {
            CaptureDecision::Capture => {
                ffi::log_info("session", "capture resumed (DMA healthy, client active)")
            }
            CaptureDecision::Backoff(BackoffReason::DmaFreeLow { free }) => ffi::log_info(
                "session",
                &format!(
                    "capture backoff: DMA free {free} B < {} B floor (plan (b))",
                    session::DMA_FREE_MIN_BYTES
                ),
            ),
            CaptureDecision::Backoff(BackoffReason::DmaLargestLow { largest }) => ffi::log_info(
                "session",
                &format!(
                    "capture backoff: DMA largest block {largest} B < {} B floor (plan (b))",
                    session::DMA_LARGEST_MIN_BYTES
                ),
            ),
            CaptureDecision::Backoff(BackoffReason::Cooldown { until_ms }) => ffi::log_info(
                "session",
                &format!(
                    "capture backoff: slow-client cooldown until {until_ms} ms (plan (c), frames dropped not queued)"
                ),
            ),
            CaptureDecision::Suspend => ffi::log_info(
                "session",
                "capture suspended: no client connected (plan (c))",
            ),
        }
    }
}

#[cfg(target_os = "espidf")]
#[allow(unused_imports)] // consumed by the Todo 20 boot wiring
pub use esp_impl::{bring_up, ensure_netif_initialised, NetHandles};

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn credentials_validate_the_wpa2_limits() {
        assert_eq!(
            WifiCredentials::new("EXAMPLE_SSID", "EXAMPLE_PASSWORD").validate(),
            Ok(())
        );
        assert_eq!(
            WifiCredentials::new("", "EXAMPLE_PASSWORD").validate(),
            Err(NetError::SsidInvalid { len: 0 })
        );
        assert_eq!(
            WifiCredentials::new("s".repeat(33), "EXAMPLE_PASSWORD").validate(),
            Err(NetError::SsidInvalid { len: 33 })
        );
        assert_eq!(
            WifiCredentials::new("EXAMPLE_SSID", "short").validate(),
            Err(NetError::PasswordInvalid { len: 5 })
        );
        // 64 hex digits = raw PSK, valid.
        let psk = "ab".repeat(32);
        assert_eq!(WifiCredentials::new("EXAMPLE_SSID", psk).validate(), Ok(()));
        // 65 bytes, never valid.
        assert_eq!(
            WifiCredentials::new("EXAMPLE_SSID", "a".repeat(65)).validate(),
            Err(NetError::PasswordInvalid { len: 65 })
        );
        assert_eq!(
            WifiCredentials::new("EXAMPLE_SSID", "z".repeat(64)).validate(),
            Err(NetError::PasswordInvalid { len: 64 })
        ); // 64 bytes but not hex, so neither a passphrase nor a raw PSK
    }

    #[test]
    fn default_net_config_validates_and_carries_plan_defaults() {
        let mut cfg = NetConfig::with_defaults();
        assert!(cfg.validate().is_err()); // default credentials are empty by design
        cfg.credentials = WifiCredentials::new("EXAMPLE_SSID", "EXAMPLE_PASSWORD");
        assert_eq!(cfg.validate(), Ok(()));
        assert_eq!(cfg.hostname, "esp32s3-av");
        assert_eq!(cfg.rtsp_port, 554);
        assert_eq!(cfg.sntp_primary, "pool.ntp.org");
        assert!(!cfg.sntp_fallback.is_empty());
        assert_eq!(cfg.cooldown_ms, crate::session::DEFAULT_COOLDOWN_MS);
    }

    #[test]
    fn retry_backoff_grows_then_caps() {
        assert_eq!(retry_backoff_ms(1), 1_000);
        assert_eq!(retry_backoff_ms(2), 2_000);
        assert_eq!(retry_backoff_ms(3), 4_000);
        assert_eq!(retry_backoff_ms(4), 8_000);
        assert_eq!(retry_backoff_ms(5), 16_000);
        assert_eq!(retry_backoff_ms(6), 30_000);
        assert_eq!(retry_backoff_ms(100), 30_000);
        // Attempt 0 (pre-first-loss steady state) still returns the base.
        assert_eq!(retry_backoff_ms(0), 1_000);
    }

    #[test]
    fn sntp_unsynced_suppression_predicate_holds_until_a_real_sync() {
        let mut clock = SntpClock::new();
        // Never started, never synced: Todo 13 must suppress SRs.
        assert!(!clock.is_synced());
        assert!(!clock.may_send_rtcp());
        assert_eq!(clock.last_sync_utc(), None);

        clock.set_started(1_000);
        // Before the timeout: no failure line yet.
        assert_eq!(clock.take_failure_report(1_000 + FAILURE_LOG_AFTER_MS - 1), None);
        assert_eq!(
            clock.take_failure_report(1_000 + FAILURE_LOG_AFTER_MS),
            Some(SntpFailureReason::NoResponseWithinTimeout)
        );
        // "Logged once" — the second tick is silent.
        assert_eq!(clock.take_failure_report(9_999_999), None);

        // A real sync flips the predicate; SRs become allowed.
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        clock.record_sync(at);
        assert!(clock.is_synced());
        assert!(clock.may_send_rtcp());
        assert_eq!(clock.last_sync_utc(), Some(at));
        // Synced before the fallback window: no swap.
        assert!(!clock.arm_fallback_once(1_000 + FALLBACK_AFTER_MS));
    }

    #[test]
    fn sntp_fallback_arms_once_after_the_window() {
        let mut clock = SntpClock::new();
        clock.set_started(5_000);
        assert!(!clock.arm_fallback_once(5_000 + FALLBACK_AFTER_MS - 1));
        assert!(clock.arm_fallback_once(5_000 + FALLBACK_AFTER_MS));
        // One-shot even if the timeout keeps passing.
        assert!(!clock.arm_fallback_once(9_999_999));
        assert!(clock.fallback_armed());
    }

    #[test]
    fn set_started_is_first_wins() {
        let mut clock = SntpClock::new();
        clock.set_started(100);
        clock.set_started(200);
        // A re-entrant supervisor must not push the timeout window forward.
        assert_eq!(clock.started_at_ms, Some(100));
    }

    #[test]
    fn resync_interval_matches_the_plan_60_minutes() {
        assert_eq!(RESYNC_INTERVAL_SECS, 60 * 60);
    }

    #[test]
    fn module_level_clock_helpers_do_not_leak_state_across_tests() {
        // The free functions touch the process-wide static; restore it so the
        // suite stays order-independent (plan fact 3).
        with_sntp_clock(|c| {
            *c = SntpClock::new();
            assert!(!c.is_synced());
            c.record_sync(SystemTime::UNIX_EPOCH);
            assert!(c.is_synced());
            *c = SntpClock::new();
            assert!(!c.is_synced());
        });
        // The wrapper must be usable concurrently with the free functions.
        let _ = is_synced();
        let _ = last_sync_utc();
        let _ = may_send_rtcp();
    }
}
