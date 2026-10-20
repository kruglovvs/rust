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

const FRAME_RATE: u32 = 10;
const FRAME_PERIOD: Duration = Duration::from_millis(100);
const RECORD_DIR: &str = "rec";

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
        Some(_) => match Service::spawn(554) {
            Ok(service) => {
                crate::log!("boot: RTSP ready at {}", service.local_addr());
                Some(service)
            }
            Err(error) => {
                crate::log!("boot: WARNING: RTSP unavailable: {error}");
                None
            }
        },
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
    let ssid = std::env::var("ESP32_WIFI_SSID").ok()?;
    let password = std::env::var("ESP32_WIFI_PASSWORD").ok()?;
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
