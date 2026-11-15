use std::thread;
use std::time::Duration;

use esp_idf_hal::peripherals::Peripherals;

use net::{self, NetConfig, WifiCredentials};
use storage::{self, Storage};

// `settings.rs` sits beside this file, and a `mod` declared inside `app.rs`
// would otherwise be looked up under `src/app/settings.rs`.
#[path = "settings.rs"]
mod settings;
use settings::*;

const RTSP_PORT: u16 = 554;

pub fn run() -> ! {
    if !RECORD_MODE_IS_KNOWN {
        logging::log!(
            "boot: WARNING: RECORD_MODE={:?} is not one of both|stream|record; using {}",
            RECORD_MODE_VALUE.unwrap_or(""),
            RECORD_MODE.as_str()
        );
    }

    if let Err(error) = net::ensure_netif_initialised() {
        logging::log!("boot: WARNING: netif initialization failed: {error}");
    }

    #[cfg(feature = "rtsp-selftest")]
    raise_rtsp_log_level();

    let peripherals = match Peripherals::take() {
        Ok(peripherals) => peripherals,
        Err(error) => fatal("peripheral initialization failed", error),
    };
    let Peripherals {
        pins,
        modem,
        spi2,
        ..
    } = peripherals;
    let esp_idf_hal::gpio::Pins {
        gpio3,
        gpio7,
        gpio8,
        gpio9,
        gpio21,
        ..
    } = pins;

    let spi = match storage::new_bus(spi2, gpio7, gpio9, gpio8) {
        Ok(spi) => Some(spi),
        Err(error) => {
            logging::log!("boot: WARNING: SD SPI initialization failed: {error}");
            None
        }
    };

    let storage = match spi.as_ref() {
        Some(spi) => match Storage::bringup(spi, storage::CandidatePins::new(gpio21, gpio3)) {
            Ok(storage) => {
                logging::log!(
                    "boot: storage ready at {} ({} kHz)",
                    storage.base_path(),
                    storage.negotiated_khz()
                );
                Some(storage)
            }
            Err(error) => {
                logging::log!("boot: WARNING: SD storage unavailable: {error}");
                None
            }
        },
        None => None,
    };

    let _network = match network_config() {
        Some(config) => match net::bring_up(modem, &config) {
            Ok(handles) => Some(handles),
            Err(error) => {
                logging::log!("boot: WARNING: network unavailable: {error}");
                None
            }
        },
        None => {
            logging::log!("boot: Wi-Fi credentials unavailable; network disabled");
            None
        }
    };

    let mount_point = storage
        .as_ref()
        .map_or("", |storage| storage.base_path());
    const IP_WAIT: Duration = Duration::from_secs(60);
    let wait_started = std::time::Instant::now();
    let mut station_ip = net::station_ipv4();
    let mut last_note = 0u64;
    while station_ip.is_none() && wait_started.elapsed() < IP_WAIT {
        thread::sleep(Duration::from_millis(500));
        let elapsed = wait_started.elapsed().as_secs();
        if elapsed >= last_note + 5 {
            last_note = elapsed;
            logging::log!("boot: waiting for station IP ({elapsed}s elapsed)");
        }
        station_ip = net::station_ipv4();
    }
    match station_ip {
        Some(_) => logging::log!("boot: station IP ready after {}s", wait_started.elapsed().as_secs()),
        None => logging::log!("boot: WARNING: no station IP after 60s; starting pipeline anyway"),
    }
    let ipv4 = station_ip.map(|ip| ip.to_string());
    let address = ipv4.as_deref().unwrap_or("0.0.0.0");

    // The status document reports whether this boot records and streams. The
    // pipeline applies the same `RECORD_MODE` and mount predicate, and it is
    // known before the pipeline starts, which is when the server is given it.
    let recording_enabled =
        cfg!(feature = "avi-recording") && !mount_point.is_empty() && RECORD_MODE.records();
    let rtsp_running = RECORD_MODE.streams();
    let fileserver_port = FILE_SERVER_PORT as u16;
    let _fileserver = fileserver::start(fileserver::Config {
        port: fileserver_port,
        mount_point: storage::BASE_PATH,
        rtsp_port: RTSP_PORT,
        rtsp_path: "/live",
        rtsp_running,
        recording_enabled,
    })
    .map_err(|error| logging::log!("boot: WARNING: fileserver unavailable: {error}"))
    .ok();

    let media_record_mode = match RECORD_MODE {
        settings::RecordMode::Both => media::RecordMode::Both,
        settings::RecordMode::Stream => media::RecordMode::Stream,
        settings::RecordMode::Record => media::RecordMode::Record,
    };
    let media_config = media::Config {
        ipv4: ipv4.as_deref(),
        mount_point,
        record_mode: media_record_mode,
        port: RTSP_PORT,
        width: VIDEO_WIDTH,
        height: VIDEO_HEIGHT,
        fps: VIDEO_FPS,
        jpeg_quality: VIDEO_JPEG_QUALITY,
        audio_sample_rate: AUDIO_SAMPLE_RATE,
        audio_bitrate: AUDIO_BITRATE,
    };
    let pipeline = match media::Pipeline::start(&media_config) {
        Ok(pipeline) => Some(pipeline),
        Err(error) => {
            logging::log!("boot: WARNING: GMF pipeline unavailable: {error}");
            None
        }
    };
    let recording = pipeline.as_ref().is_some_and(|pipeline| pipeline.recording());
    let audio = pipeline.as_ref().is_some_and(|pipeline| pipeline.audio());

    logging::log!(
        "boot: ready rtsp=rtsp://{address}:{RTSP_PORT}/live http=http://{address}:{fileserver_port}/ \
         recording={} audio={} capture={VIDEO_WIDTH}x{VIDEO_HEIGHT}@{VIDEO_FPS}fps",
        on_off(recording),
        on_off(audio)
    );

    #[cfg(feature = "rtsp-selftest")]
    {
        log_heap_after_pipeline();
        if pipeline.is_some() {
            logging::log!("boot: selftest begin");
            crate::selftest::probe(RTSP_PORT, station_ip);
            logging::log!("boot: selftest end");
        }
    }

    #[cfg(feature = "rtsp-selftest")]
    {
        let mut tick: u32 = 0;
        loop {
            thread::sleep(Duration::from_secs(1));
            tick = tick.wrapping_add(1);
            if tick % 10 == 0 {
                let (free, largest, spiram) = heap_sample();
                logging::log!("boot: heartbeat internal_free={free} largest={largest} spiram_free={spiram}");
                log_recordings();
            }
        }
    }
    #[cfg(not(feature = "rtsp-selftest"))]
    loop {
        thread::sleep(Duration::from_secs(1));
    }
}

fn on_off(value: bool) -> &'static str {
    if value {
        "on"
    } else {
        "off"
    }
}

fn network_config() -> Option<NetConfig> {
    let ssid = WIFI_SSID?.to_owned();
    let password = WIFI_PASSWORD?.to_owned();
    let credentials = WifiCredentials::new(ssid, password);
    if let Err(error) = credentials.validate() {
        logging::log!("boot: WARNING: invalid Wi-Fi credentials: {error}");
        return None;
    }

    let mut config = NetConfig::with_defaults();
    config.credentials = credentials;
    Some(config)
}

fn fatal<E: std::fmt::Display>(message: &str, error: E) -> ! {
    logging::log!("boot: FATAL: {message}: {error}");
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}

#[cfg(feature = "rtsp-selftest")]
fn raise_rtsp_log_level() {
    const TAGS: [&[u8]; 7] = [
        b"ESP_RTSP_SERVER\0",
        b"ESP_RTSP\0",
        b"ESP_RTSP_SERVICE\0",
        b"RTSP\0",
        b"RTC_TRANSPORT\0",
        b"ESP_SERVICE\0",
        b"MEDIA_LIB\0",
    ];
    for tag in TAGS {
        // SAFETY: each tag is a NUL-terminated static C string.
        unsafe {
            esp_idf_svc::sys::esp_log_level_set(
                tag.as_ptr() as *const core::ffi::c_char,
                esp_idf_svc::sys::esp_log_level_t_ESP_LOG_DEBUG,
            );
        }
    }
}

#[cfg(feature = "rtsp-selftest")]
fn log_recordings() {
    let dir = format!("{}/rec", storage::BASE_PATH);
    match std::fs::read_dir(&dir) {
        Ok(entries) => {
            let mut found = false;
            for entry in entries.flatten() {
                let name = entry.file_name();
                let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
                logging::log!("rec: file={:?} bytes={len}", name.to_string_lossy());
                found = true;
            }
            if !found {
                logging::log!("rec: {dir} is empty");
            }
        }
        Err(error) => logging::log!("rec: listing {dir} failed: {error}"),
    }
}

#[cfg(feature = "rtsp-selftest")]
fn log_heap_after_pipeline() {
    let (internal_free, internal_largest, spiram_free) = heap_sample();
    logging::log!(
        "boot: heap after pipeline internal_free={internal_free} internal_largest={internal_largest} spiram_free={spiram_free}"
    );
}

#[cfg(feature = "rtsp-selftest")]
fn heap_sample() -> (usize, usize, usize) {
    const MALLOC_CAP_INTERNAL: u32 = 1 << 11;
    const MALLOC_CAP_SPIRAM: u32 = 1 << 10;
    use esp_idf_svc::sys::{heap_caps_get_free_size, heap_caps_get_largest_free_block};
    // SAFETY: both calls only read allocator statistics and retain no pointers.
    unsafe {
        (
            heap_caps_get_free_size(MALLOC_CAP_INTERNAL),
            heap_caps_get_largest_free_block(MALLOC_CAP_INTERNAL),
            heap_caps_get_free_size(MALLOC_CAP_SPIRAM),
        )
    }
}
