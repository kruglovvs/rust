use std::thread;
use std::time::Duration;

use esp_idf_hal::peripherals::Peripherals;

use net::{self, NetConfig, WifiCredentials};
use storage::{self, Storage};

include!(concat!(env!("OUT_DIR"), "/wifi_credentials.rs"));

const RTSP_PORT: u16 = 554;

pub fn run() -> ! {
    if let Err(error) = net::ensure_netif_initialised() {
        logging::log!("boot: WARNING: netif initialization failed: {error}");
    }

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
    let media_config = media::Config {
        ipv4: None,
        mount_point,
        port: RTSP_PORT,
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
    let rtsp = pipeline.as_ref().is_some_and(|pipeline| pipeline.rtsp());

    logging::log!("boot: ready recording={recording} audio={audio} rtsp={rtsp}");

    loop {
        thread::sleep(Duration::from_secs(1));
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
