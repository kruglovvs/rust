use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use esp_idf_hal::peripherals::Peripherals;

use crate::camera::{Camera, CameraConfig};
use crate::mic::{AudioFrame, MicCapture, SAMPLE_RATE_HZ};
use crate::muxer::{AudioTrack, VideoTrack};
use crate::net::{self, NetConfig, WifiCredentials};
use crate::recorder::{DirFactory, Recorder, RecorderConfig};
use crate::rtsp::service::Service;
use crate::sensor;
use crate::storage::{self, Storage};

include!(concat!(env!("OUT_DIR"), "/wifi_credentials.rs"));

const FRAME_RATE: u32 = 10;
const FRAME_PERIOD: Duration = Duration::from_millis(100);
const RECORD_DIR: &str = "rec";

/// RTSP bind attempts before giving up (transient LwIP pool pressure while
/// Wi-Fi associates is common on the first boot after reset).
const SPAWN_ATTEMPTS: u32 = 3;
const SPAWN_RETRY_DELAY: Duration = Duration::from_millis(250);
/// Internal SRAM below which the two UDP socket allocations are skipped so
/// the TCP RTSP listener still exists (bug-mindmap B2).
const LOW_INTERNAL_HEAP_BYTES: usize = 24 * 1024;

/// Private FFI boundary — the only `unsafe` in this file (house rule: every
/// executable `unsafe` lives inside `mod ffi`).
mod ffi {
    use esp_idf_svc::sys::{heap_caps_get_free_size, heap_caps_get_largest_free_block};

    // Values from ESP-IDF components/heap/include/esp_heap_caps.h; not exposed
    // as Rust constants by esp-idf-sys.
    const MALLOC_CAP_SPIRAM: u32 = 1 << 10;
    const MALLOC_CAP_INTERNAL: u32 = 1 << 11;

    /// Heap numbers that discriminate bug-mindmap B2 (internal SRAM gone) from
    /// B1/B3 (socket-table/listen-PCB limits, which leave internal heap healthy).
    pub struct HeapSnapshot {
        pub internal_free: usize,
        pub internal_largest: usize,
        pub spiram_free: usize,
    }

    pub fn heap_snapshot() -> HeapSnapshot {
        unsafe {
            HeapSnapshot {
                internal_free: heap_caps_get_free_size(MALLOC_CAP_INTERNAL),
                internal_largest: heap_caps_get_largest_free_block(MALLOC_CAP_INTERNAL),
                spiram_free: heap_caps_get_free_size(MALLOC_CAP_SPIRAM),
            }
        }
    }
}

pub fn run() -> ! {
    if let Err(error) = net::ensure_netif_initialised() {
        crate::log!("boot: WARNING: netif initialization failed: {error}");
    }

    let peripherals = match Peripherals::take() {
        Ok(peripherals) => peripherals,
        Err(error) => fatal("peripheral initialization failed", error),
    };
    let esp_idf_hal::peripherals::Peripherals { pins, modem, i2s0, spi2, .. } = peripherals;
    let esp_idf_hal::gpio::Pins {
        gpio3,
        gpio7,
        gpio8,
        gpio9,
        gpio21,
        gpio41,
        gpio42,
        ..
    } = pins;

    let mut camera = match Camera::init(CameraConfig::default()) {
        Ok(camera) => camera,
        Err(error) => fatal("camera initialization failed", error),
    };
    crate::log!("boot: camera ready");

    if let Err(error) = sensor::log_sensor_identity(
        camera.frames_grabbed(),
        camera.frames_rejected(),
    ) {
        crate::log!("boot: WARNING: sensor probe failed: {error}");
    }

    let spi = match storage::new_bus(spi2, gpio7, gpio9, gpio8) {
        Ok(spi) => Some(spi),
        Err(error) => {
            crate::log!("boot: WARNING: SD SPI initialization failed: {error}");
            None
        }
    };

    let storage = match spi.as_ref() {
        Some(spi) => match Storage::bringup(
            spi,
            storage::CandidatePins::new(gpio21, gpio3),
        ) {
            Ok(storage) => {
                crate::log!(
                    "boot: storage ready at {} ({} kHz)",
                    storage.base_path(),
                    storage.negotiated_khz()
                );
                Some(storage)
            }
            Err(error) => {
                crate::log!("boot: WARNING: SD storage unavailable: {error}");
                None
            }
        },
        None => None,
    };

    let mut recorder = storage
        .as_ref()
        .and_then(|storage| match open_recorder(storage.base_path()) {
            Ok(recorder) => Some(recorder),
            Err(error) => {
                crate::log!("boot: WARNING: recorder unavailable: {error}");
                None
            }
        });

    let (audio_tx, audio_rx) = mpsc::channel::<AudioFrame>();
    let audio_enabled = match MicCapture::new(i2s0, gpio42, gpio41) {
        Ok(mic) => match mic.spawn(audio_tx) {
            Ok(_) => {
                crate::log!("boot: microphone ready");
                true
            }
            Err(error) => {
                crate::log!("boot: WARNING: microphone task unavailable: {error}");
                false
            }
        },
        Err(error) => {
            crate::log!("boot: WARNING: microphone unavailable: {error}");
            false
        }
    };

    let network = match network_config() {
        Some(config) => match net::bring_up(modem, &config) {
            Ok(handles) => Some(handles),
            Err(error) => {
                crate::log!("boot: WARNING: network unavailable: {error}");
                None
            }
        },
        None => {
            crate::log!("boot: Wi-Fi credentials unavailable; network disabled");
            None
        }
    };

    let rtsp = match network.as_ref() {
        Some(_) => {
            let heap = ffi::heap_snapshot();
            crate::log!(
                "boot: heap before rtsp internal_free={} internal_largest={} spiram_free={}",
                heap.internal_free,
                heap.internal_largest,
                heap.spiram_free
            );
            match spawn_service_with_retry() {
                Ok(service) => {
                    crate::log!("boot: RTSP ready at {}", service.local_addr());
                    Some(service)
                }
                Err(error) => {
                    crate::log!(
                        "boot: WARNING: RTSP unavailable after {SPAWN_ATTEMPTS} attempts: {error}"
                    );
                    // B2 escape hatch: with internal SRAM nearly gone, the two
                    // UDP sockets cannot be allocated either. Keep the RTSP
                    // control endpoint alive over TCP only and say we skipped them.
                    let heap = ffi::heap_snapshot();
                    if heap.internal_free < LOW_INTERNAL_HEAP_BYTES {
                        crate::log!(
                            "boot: WARNING: internal heap low (free={} largest={}); retrying RTSP with TCP only, UDP sockets skipped",
                            heap.internal_free,
                            heap.internal_largest
                        );
                        match Service::spawn_tcp_only(554) {
                            Ok(service) => {
                                crate::log!(
                                    "boot: RTSP ready at {} (UDP sockets skipped)",
                                    service.local_addr()
                                );
                                Some(service)
                            }
                            Err(error) => {
                                crate::log!(
                                    "boot: WARNING: RTSP TCP-only bind also failed: {error}"
                                );
                                None
                            }
                        }
                    } else {
                        None
                    }
                }
            }
        }
        None => None,
    };

    let started = Instant::now();
    let mut next_video = Instant::now();
    crate::log!(
        "boot: ready recording={} audio={} rtsp={}",
        recorder.is_some(),
        audio_enabled,
        rtsp.is_some()
    );

    loop {
        if Instant::now() >= next_video {
            next_video += FRAME_PERIOD;
            let pts_us = started.elapsed().as_micros() as u64;
            match camera.grab() {
                Ok(frame) => {
                    if let Some(recorder) = recorder.as_mut() {
                        if let Err(error) = recorder.write_video_frame(pts_us, frame.as_bytes()) {
                            crate::log!("boot: recorder video write failed: {error}");
                        }
                    }
                }
                Err(error) => crate::log!("boot: camera frame dropped: {error}"),
            }
        }

        while let Ok(samples) = audio_rx.try_recv() {
            if let Some(recorder) = recorder.as_mut() {
                let pts_us = started.elapsed().as_micros() as u64;
                if let Err(error) = recorder.write_audio_samples(pts_us, &samples) {
                    crate::log!("boot: recorder audio write failed: {error}");
                }
            }
        }

        thread::sleep(Duration::from_millis(5));
    }
}

fn spawn_service_with_retry() -> std::io::Result<Service> {
    retry(SPAWN_ATTEMPTS, SPAWN_RETRY_DELAY, |attempt| {
        let result = Service::spawn(554);
        if let Err(error) = &result {
            crate::log!("boot: Service::spawn attempt {attempt}/{SPAWN_ATTEMPTS} failed: {error}");
        }
        result
    })
}

/// Run `op(attempt)` up to `attempts` times (at least once), sleeping `delay`
/// between failures, and return the last error when every attempt fails.
fn retry<T, F>(attempts: u32, delay: Duration, mut op: F) -> std::io::Result<T>
where
    F: FnMut(u32) -> std::io::Result<T>,
{
    let attempts = attempts.max(1);
    let mut attempt = 1;
    loop {
        match op(attempt) {
            Ok(value) => return Ok(value),
            Err(error) if attempt >= attempts => return Err(error),
            Err(_) => {
                thread::sleep(delay);
                attempt += 1;
            }
        }
    }
}

fn open_recorder(mount: &str) -> Result<Recorder, crate::recorder::RecorderError> {
    use crate::recorder::RecorderError;

    let video = VideoTrack::new(320, 240, FRAME_RATE)
        .map_err(|error| RecorderError::InvalidConfig(error.to_string()))?;
    let audio = AudioTrack::new(SAMPLE_RATE_HZ, 1)
        .map_err(|error| RecorderError::InvalidConfig(error.to_string()))?;
    let config = RecorderConfig::new(video, audio);
    Recorder::new(config, DirFactory::new(format!("{mount}/{RECORD_DIR}")))
}

fn network_config() -> Option<NetConfig> {
    let ssid = WIFI_SSID?.to_owned();
    let password = WIFI_PASSWORD?.to_owned();
    let credentials = WifiCredentials::new(ssid, password);
    if let Err(error) = credentials.validate() {
        crate::log!("boot: WARNING: invalid Wi-Fi credentials: {error}");
        return None;
    }

    let mut config = NetConfig::with_defaults();
    config.credentials = credentials;
    Some(config)
}

fn fatal<E: std::fmt::Display>(message: &str, error: E) -> ! {
    crate::log!("boot: FATAL: {message}: {error}");
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}

#[cfg(test)]
mod tests {
    use super::retry;
    use std::cell::Cell;
    use std::io;
    use std::time::Duration;

    fn enomem() -> io::Error {
        io::Error::from_raw_os_error(12)
    }

    #[test]
    fn retry_returns_first_success_without_extra_attempts() {
        let calls = Cell::new(0);
        let result: io::Result<u32> = retry(3, Duration::ZERO, |attempt| {
            calls.set(calls.get() + 1);
            if attempt < 2 {
                Err(enomem())
            } else {
                Ok(attempt)
            }
        });
        assert_eq!(result.expect("second attempt succeeds"), 2);
        assert_eq!(calls.get(), 2, "must stop as soon as an attempt succeeds");
    }

    #[test]
    fn retry_exhausts_attempts_and_returns_the_last_error() {
        let calls = Cell::new(0);
        let result: io::Result<()> = retry(3, Duration::ZERO, |_| {
            calls.set(calls.get() + 1);
            Err(enomem())
        });
        assert_eq!(calls.get(), 3);
        assert_eq!(result.expect_err("all attempts fail").raw_os_error(), Some(12));
    }

    #[test]
    fn retry_attempts_at_least_once_even_when_zero_is_requested() {
        let calls = Cell::new(0);
        let result: io::Result<()> = retry(0, Duration::ZERO, |_| {
            calls.set(calls.get() + 1);
            Err(enomem())
        });
        assert_eq!(calls.get(), 1);
        assert!(result.is_err());
    }

    #[test]
    fn retry_succeeds_immediately_on_first_attempt() {
        let calls = Cell::new(0);
        let result: io::Result<&str> = retry(3, Duration::ZERO, |_| {
            calls.set(calls.get() + 1);
            Ok("first")
        });
        assert_eq!(result.expect("first attempt succeeds"), "first");
        assert_eq!(calls.get(), 1);
    }
}
