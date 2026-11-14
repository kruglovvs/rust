//! GMF/ADF media pipeline for the ESP32-S3 board.
//!
//! This crate is the whole media pipeline and nothing else:
//!
//! ```text
//! esp_capture_service (SRC)
//!    ├─ video: DVP OV3660 → MJPEG 320x240 @10fps
//!    ├─ audio: PDM mic → PCM 16 kHz mono → G711U encoder (inside capture)
//!    ├─ built-in storage muxer → AVI in <mount>/rec
//!    └─ link → esp_rtsp_service (ROLE_SERVER) → rtsp://<ip>:554/live
//! ```
//!
//! Public contract:
//!
//! * [`Pipeline::start`] builds every handle (I2C bus for the camera SCCB,
//!   PDM I2S RX channel, `esp_codec_dev` record handle, audio/video sources,
//!   capture service + setup bundle, RTSP service), links capture to RTSP,
//!   and starts RTSP **then** capture. On any failure it unwinds everything
//!   already created in reverse order and returns an [`Error`] naming the
//!   failed step.
//! * [`Pipeline::stop`] stops capture then RTSP, unlinks them, and releases
//!   every owned handle. It is idempotent.
//! * [`Drop`] calls [`Pipeline::stop`].
//!
//! The only `unsafe` in the crate is inside the private [`ffi`] module.

use std::ffi::CString;
use std::fmt;

use esp_idf_sys as sys;
use esp_idf_sys::gmf;

/// Wide-open capture service name.
const CAPTURE_SERVICE_NAME: &[u8] = b"capture\0";
/// Wide-open RTSP service name.
const RTSP_SERVICE_NAME: &[u8] = b"rtsp\0";
/// Recording sink service name.
const MUXER_SERVICE_NAME: &[u8] = b"muxer\0";
/// RTSP path shared by the server URL.
const RTSP_PATH: &str = "/live";
/// Default stream id (`ESP_MEDIA_DEFAULT_STREAM`).
const DEFAULT_STREAM: u16 = 0;
/// Directory created under the mount point for AVI recordings.
const RECORD_DIR: &str = "rec";
/// AVI recording is opt-in: the muxer sink requests the provider's global
/// cache, which currently starves the RTSP sink of video frames.
const AVI_RECORDING: bool = cfg!(feature = "avi-recording");
/// PDM microphone clock pin.
const MIC_CLK_GPIO: i32 = 42;
/// PDM microphone data pin.
const MIC_DIN_GPIO: i32 = 41;
/// Camera SCCB SDA pin.
const SCCB_SDA_GPIO: i32 = 40;
/// Camera SCCB SCL pin.
const SCCB_SCL_GPIO: i32 = 39;
/// G.711 µ-law sample rate. RTP payload type 0 is fixed at 8000 Hz by
/// RFC 3551, so the track and the payload clock must agree; a 16 kHz track
/// makes every client play the stream at half speed.
const AUDIO_SAMPLE_RATE_HZ: u32 = 8_000;
/// AVI muxer aligned internal-RAM write cache (16 KiB, as recommended).
const MUXER_RAM_CACHE: u32 = 16 * 1024;

/// Pipeline configuration supplied by the runtime.
pub struct Config<'a> {
    /// Station IPv4 address advertised by the RTSP service. `None` binds
    /// `0.0.0.0` as the RTSP stack default.
    pub ipv4: Option<&'a str>,
    /// Mount point of the SD card (`"/sdcard"`). An empty string disables file
    /// recording so streaming still works.
    pub mount_point: &'a str,
    /// RTSP local port.
    pub port: u16,
    /// Requested video width in pixels (sensor/pipeline negotiate the closest
    /// supported size; 640x480 is VGA, 320x240 is QVGA).
    pub width: u32,
    /// Requested video height in pixels.
    pub height: u32,
    /// Requested frame rate in frames per second.
    pub fps: u32,
}

impl Config<'_> {
    /// Reject geometry the sensor and pipeline cannot honour.
    pub fn validate(&self) -> Result<(), Error> {
        if self.width == 0 || self.height == 0 {
            return Err(Error::InvalidConfig("width and height must be non-zero"));
        }
        if self.width % 2 != 0 || self.height % 2 != 0 {
            return Err(Error::InvalidConfig("width and height must be even"));
        }
        if self.fps == 0 || self.fps > 30 {
            return Err(Error::InvalidConfig("fps must be in 1..=30"));
        }
        Ok(())
    }
}

/// One discrete pipeline construction/teardown step, used to name failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    I2cBus,
    I2sChannel,
    I2sPdmMode,
    I2sDataInterface,
    CodecDevice,
    AudioSource,
    VideoSource,
    MediaLibAdapter,
    Scheduler,
    RegisterAudioEncoder,
    RegisterVideoEncoder,
    RegisterMuxer,
    CaptureService,
    CaptureSetup,
    CaptureSourceConfig,
    CaptureTrackVideo,
    CaptureTrackAudio,
    CaptureMuxerConfig,
    CaptureSetupApply,
    CaptureSetupDestroy,
    MuxerService,
    MuxerSetup,
    MuxerLink,
    MuxerStart,
    RtspService,
    RtspSetup,
    RtspUrl,
    RtspIp,
    MediaLink,
    RtspStart,
    CaptureStart,
}

impl Step {
    /// Stable snake_case name used in logs and `Display`.
    pub fn as_str(self) -> &'static str {
        match self {
            Step::I2cBus => "i2c_bus",
            Step::I2sChannel => "i2s_channel",
            Step::I2sPdmMode => "i2s_pdm_mode",
            Step::I2sDataInterface => "i2s_data_interface",
            Step::CodecDevice => "codec_device",
            Step::AudioSource => "audio_source",
            Step::VideoSource => "video_source",
            Step::MediaLibAdapter => "media_lib_adapter",
            Step::Scheduler => "scheduler",
            Step::RegisterAudioEncoder => "register_audio_encoder",
            Step::RegisterVideoEncoder => "register_video_encoder",
            Step::RegisterMuxer => "register_muxer",
            Step::CaptureService => "capture_service",
            Step::CaptureSetup => "capture_setup",
            Step::CaptureSourceConfig => "capture_source_config",
            Step::CaptureTrackVideo => "capture_track_video",
            Step::CaptureTrackAudio => "capture_track_audio",
            Step::CaptureMuxerConfig => "capture_muxer_config",
            Step::CaptureSetupApply => "capture_setup_apply",
            Step::CaptureSetupDestroy => "capture_setup_destroy",
            Step::MuxerService => "muxer_service",
            Step::MuxerSetup => "muxer_setup",
            Step::MuxerLink => "muxer_link",
            Step::MuxerStart => "muxer_start",
            Step::RtspService => "rtsp_service",
            Step::RtspSetup => "rtsp_setup",
            Step::RtspUrl => "rtsp_url",
            Step::RtspIp => "rtsp_ip",
            Step::MediaLink => "media_link",
            Step::RtspStart => "rtsp_start",
            Step::CaptureStart => "capture_start",
        }
    }
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Pipeline construction error. `Display` always names the failed step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// An `esp_err_t`-returning call failed.
    Esp { step: Step, code: i32 },
    /// A constructor returned NULL.
    Null { step: Step },
    /// A configuration string could not be converted to a `CString`.
    Config { step: Step },
    /// A configuration value is outside the supported range.
    InvalidConfig(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Esp { step, code } => {
                write!(f, "media pipeline: step `{step}` failed (esp_err_t={code})")
            }
            Error::Null { step } => {
                write!(f, "media pipeline: step `{step}` returned a null handle")
            }
            Error::Config { step } => {
                write!(f, "media pipeline: step `{step}` received an invalid configuration string")
            }
            Error::InvalidConfig(reason) => {
                write!(f, "media pipeline: invalid configuration: {reason}")
            }
        }
    }
}

impl std::error::Error for Error {}

/// Map a service thread name to an override `(stack_size, priority, core_id)`.
///
/// Mirrors Espressif's `esp_rtsp_service` example scheduler: RTSP threads get
/// 8 KiB stacks on core 1, capture/GMF threads get 8 KiB stacks on core 0.
/// `None` keeps the vendor defaults (the callback reports `ESP_ERR_NOT_FOUND`).
fn thread_override(name: &str) -> Option<(u32, u8, i8)> {
    match name {
        "rtsp_server" | "rtsp_push" | "rtsp_src" => Some((8192, 1, 1)),
        _ if name.contains("muxer") => Some((8192, 5, 1)),
        _ if name.contains("capture")
            || name.contains("venc")
            || name.contains("aenc")
            || name.contains("VID_SRC")
            || name.contains("AUD_SRC") =>
        {
            Some((8192, 8, 0))
        }
        _ => None,
    }
}

/// Build the MJPEG track description from the requested geometry.
fn video_track_info(cfg: &Config<'_>) -> gmf::esp_media_track_info_t {
    gmf::esp_media_track_info_t {
        id: 1,
        type_: gmf::esp_media_track_type_t_ESP_MEDIA_TRACK_TYPE_VIDEO,
        info: gmf::esp_media_track_info_t__bindgen_ty_1 {
            video: gmf::esp_media_video_info_t {
                codec: gmf::esp_capture_format_id_t_ESP_CAPTURE_FMT_ID_MJPEG as u32,
                width: cfg.width as u16,
                height: cfg.height as u16,
                fps: cfg.fps as u16,
                bitrate: 0,
            },
        },
    }
}

/// Build the G.711 µ-law track description.
fn audio_track_info() -> gmf::esp_media_track_info_t {
    gmf::esp_media_track_info_t {
        id: 2,
        type_: gmf::esp_media_track_type_t_ESP_MEDIA_TRACK_TYPE_AUDIO,
        info: gmf::esp_media_track_info_t__bindgen_ty_1 {
            audio: gmf::esp_media_audio_info_t {
                codec: gmf::esp_capture_format_id_t_ESP_CAPTURE_FMT_ID_G711U as u32,
                sample_rate: AUDIO_SAMPLE_RATE_HZ,
                bits_per_sample: 16,
                channel: 1,
                bitrate: 64_000,
            },
        },
    }
}

/// Owns every handle of the running media pipeline.
pub struct Pipeline {
    rtsp: *mut gmf::esp_rtsp_service_t,
    capture: *mut gmf::esp_capture_service_t,
    muxer: *mut gmf::esp_muxer_service_t,
    dev_handle: gmf::esp_codec_dev_handle_t,
    data_if: *const gmf::audio_codec_data_if_t,
    i2s_rx: sys::i2s_chan_handle_t,
    i2c_bus: sys::i2c_master_bus_handle_t,
    linked: bool,
    muxer_linked: bool,
    /// The RTSP start thread returned `Ok` (known only if it ever reports back).
    rtsp_started: bool,
    /// The RTSP start thread has been spawned and may still be inside start.
    rtsp_start_pending: bool,
    capture_started: bool,
    muxer_started: bool,
    registered: bool,
}

impl Pipeline {
    /// Build and start the full pipeline.
    pub fn start(cfg: &Config<'_>) -> Result<Self, Error> {
        cfg.validate()?;
        let mut pipeline = Pipeline {
            rtsp: std::ptr::null_mut(),
            capture: std::ptr::null_mut(),
            muxer: std::ptr::null_mut(),
            dev_handle: std::ptr::null_mut(),
            data_if: std::ptr::null(),
            i2s_rx: std::ptr::null_mut(),
            i2c_bus: std::ptr::null_mut(),
            linked: false,
            muxer_linked: false,
            rtsp_started: false,
            rtsp_start_pending: false,
            capture_started: false,
            muxer_started: false,
            registered: false,
        };

        match pipeline.start_inner(cfg) {
            Ok(()) => Ok(pipeline),
            Err(error) => {
                pipeline.stop();
                Err(error)
            }
        }
    }

    /// Stop and release every handle. Safe to call more than once.
    pub fn stop(&mut self) {
        ffi::stop(self);
    }

    /// Whether AVI recording to the SD card is configured.
    pub fn recording(&self) -> bool {
        self.muxer_started
    }

    /// Whether the audio source (PDM microphone + codec device) is present.
    pub fn audio(&self) -> bool {
        !self.dev_handle.is_null()
    }

    /// Whether the RTSP server is running.
    pub fn rtsp(&self) -> bool {
        self.rtsp_started || self.rtsp_start_pending
    }

    fn start_inner(&mut self, cfg: &Config<'_>) -> Result<(), Error> {
        // 00. Install the default media_lib adapter before anything else: the
        //     RTSP core calls media_lib_socket_*, which are no-ops returning
        //     ESP_ERR_NOT_SUPPORTED until the ESP-IDF socket implementation is
        //     registered here.
        logging::log!("pipeline: 00 media_lib adapter");
        ffi::install_media_lib_adapter()?;

        // 0. Install the service scheduler first: it must be in place before
        //    any service is created, or the RTSP threads start with the 4 KiB
        //    default stack and starve the application thread.
        logging::log!("pipeline: 0 scheduler");
        ffi::install_scheduler()?;

        // 1. Camera SCCB bus: esp32-camera's sccb-ng uses the *new* I2C master
        //    driver, so the bus must exist before esp_capture_start.
        logging::log!("pipeline: 1 i2c bus");
        self.i2c_bus = ffi::i2c_master_bus()?;

        // 2. PDM microphone I2S RX channel, initialized but deliberately NOT
        //    enabled: esp_codec_dev_open() enables it exactly once via the
        //    I2S data interface's set_fmt/enable path.
        logging::log!("pipeline: 2 pdm i2s");
        self.i2s_rx = ffi::pdm_rx_channel()?;

        // 3. esp_codec_dev record device backed by the I2S data interface.
        logging::log!("pipeline: 3 codec device");
        let (dev_handle, data_if) = ffi::codec_device(self.i2s_rx)?;
        self.dev_handle = dev_handle;
        self.data_if = data_if;

        // 4. Audio source (PCM; G711U is encoded downstream by the capture
        //    service's GMF audio path).
        logging::log!("pipeline: 4 audio source");
        let audio_src = ffi::audio_source(self.dev_handle)?;

        // 5. DVP video source (OV3660, MJPEG).
        logging::log!("pipeline: 5 video source");
        let video_src = ffi::video_source()?;

        // 6. Register the default muxer and encoders. esp_capture resolves
        //    them through a registry (esp_muxer_open, encoder lookups), so
        //    they must be present before the capture service is created.
        logging::log!("pipeline: 6 register defaults");
        ffi::register_defaults()?;
        self.registered = true;

        // 7. Capture service.
        logging::log!("pipeline: 7 capture service");
        let service_cfg = gmf::esp_capture_service_cfg_t {
            name: CAPTURE_SERVICE_NAME.as_ptr() as *const core::ffi::c_char,
            max_stream_num: 1,
        };
        self.capture = ffi::capture_service(&service_cfg)?;

        // 8. Setup bundle: sources, tracks, muxer; apply and destroy.
        logging::log!("pipeline: 8 capture setup apply");
        ffi::configure_capture(self.capture, &service_cfg, audio_src, video_src, cfg)?;

        // 8b. Recording sink. esp_capture_service's built-in muxer has no AVI
        //     support, so recording uses esp_muxer_service instead.
        if AVI_RECORDING && !cfg.mount_point.is_empty() {
            logging::log!("pipeline: 8b muxer service");
            self.muxer = ffi::muxer_service(cfg)?;
            logging::log!("pipeline: 8c muxer link");
            ffi::link_muxer(self.capture, self.muxer)?;
            self.muxer_linked = true;
        }

        // 9. RTSP service.
        logging::log!("pipeline: 9 rtsp service");
        let rtsp_cfg = gmf::esp_rtsp_service_cfg_t {
            name: RTSP_SERVICE_NAME.as_ptr() as *const core::ffi::c_char,
            role: gmf::esp_rtsp_service_role_t_ESP_RTSP_SERVICE_ROLE_SERVER,
        };
        self.rtsp = ffi::rtsp_service(&rtsp_cfg, cfg)?;

        // 10. Join capture -> RTSP over the default stream, no frame plumbing.
        logging::log!("pipeline: 10 media link");
        ffi::link(self.capture, self.rtsp)?;
        self.linked = true;

        // 11. Start capture before the consumers: the capture provider's tracks
        // are only queryable while running, and both the RTSP `on_start` hook
        // and the muxer's track configuration ask the linked provider for them.
        logging::log!("pipeline: 11 start capture");
        ffi::start_capture(self.capture)?;
        self.capture_started = true;

        if !self.muxer.is_null() {
            logging::log!("pipeline: 11a start muxer");
            ffi::start_muxer(self.muxer)?;
            self.muxer_started = true;
        }

        logging::log!("pipeline: 12 start rtsp (async)");
        let rtsp_ptr = self.rtsp as usize;
        std::thread::Builder::new()
            .name("rtsp-start".into())
            .stack_size(16 * 1024)
            .spawn(move || {
                let result = ffi::start_rtsp(rtsp_ptr as *mut gmf::esp_rtsp_service_t);
                match result {
                    Ok(()) => logging::log!("pipeline: rtsp start returned ok"),
                    Err(error) => logging::log!("pipeline: rtsp start returned err={error}"),
                }
            })
            .map_err(|_| Error::Esp {
                step: Step::RtspStart,
                code: -1,
            })?;
        self.rtsp_start_pending = true;

        logging::log!("pipeline: started");
        Ok(())
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The only `unsafe` in this crate: every raw ESP-IDF/GMF call lives here.
mod ffi {
    use super::*;
    use core::ffi::c_char;
    use core::ffi::CStr;
    use core::ptr::{null, null_mut};

    /// Convert an `esp_err_t` into a step-named result.
    pub(crate) fn check(code: i32, step: Step) -> Result<(), Error> {
        if code == sys::ESP_OK {
            Ok(())
        } else {
            Err(Error::Esp { step, code })
        }
    }

    /// Vendor-equivalent service thread sizing (see the esp_rtsp_service example).
    unsafe extern "C" fn scheduler_cb(
        request: *const gmf::esp_service_thread_request_t,
        cfg: *mut gmf::esp_service_thread_cfg_t,
        _ctx: *mut core::ffi::c_void,
    ) -> i32 {
        if request.is_null() || cfg.is_null() {
            return sys::ESP_ERR_INVALID_ARG;
        }
        let name = (*request).thread_name;
        if name.is_null() {
            return sys::ESP_ERR_NOT_FOUND;
        }
        let name = match CStr::from_ptr(name).to_str() {
            Ok(name) => name,
            Err(_) => return sys::ESP_ERR_NOT_FOUND,
        };
        match thread_override(name) {
            Some((stack_size, priority, core_id)) => {
                (*cfg).stack_size = stack_size;
                (*cfg).priority = priority;
                (*cfg).core_id = core_id;
                (*cfg).is_ext = false;
                logging::log!(
                    "scheduler: thread={} stack={} prio={} core={}",
                    name,
                    stack_size,
                    priority,
                    core_id
                );
                sys::ESP_OK
            }
            None => sys::ESP_ERR_NOT_FOUND,
        }
    }

    /// Install the default media_lib adapter (socket implementation).
    pub fn install_media_lib_adapter() -> Result<(), Error> {
        let code = unsafe { gmf::media_lib_add_default_adapter() };
        check(code, Step::MediaLibAdapter)
    }

    /// Install the service scheduler callback before any service is created.
    pub fn install_scheduler() -> Result<(), Error> {
        let code = unsafe { gmf::esp_service_scheduler_set_cb(Some(scheduler_cb), null_mut()) };
        check(code, Step::Scheduler)
    }

    /// I2C master bus on port 0 (SCCB SDA 40, SCL 39) using the new driver.
    pub fn i2c_master_bus() -> Result<sys::i2c_master_bus_handle_t, Error> {
        let config = i2c_config();
        let mut bus: sys::i2c_master_bus_handle_t = null_mut();
        let code = unsafe { sys::i2c_new_master_bus(&config, &mut bus) };
        check(code, Step::I2cBus)?;
        if bus.is_null() {
            return Err(Error::Null { step: Step::I2cBus });
        }
        Ok(bus)
    }

    pub(crate) fn i2c_config() -> sys::i2c_master_bus_config_t {
        let mut config = sys::i2c_master_bus_config_t::default();
        config.i2c_port = 0;
        config.sda_io_num = SCCB_SDA_GPIO;
        config.scl_io_num = SCCB_SCL_GPIO;
        config.glitch_ignore_cnt = 7;
        config.intr_priority = 0;
        config.trans_queue_depth = 0;
        config.__bindgen_anon_1.clk_source =
            sys::soc_periph_i2c_clk_src_t_I2C_CLK_SRC_DEFAULT;
        config.flags.set_enable_internal_pullup(1);
        config
    }

    pub(crate) fn pdm_config() -> sys::i2s_pdm_rx_config_t {
        let mut clk = sys::i2s_pdm_rx_clk_config_t::default();
        clk.sample_rate_hz = 16_000;
        clk.clk_src = sys::soc_periph_i2s_clk_src_t_I2S_CLK_SRC_DEFAULT;
        clk.mclk_multiple = sys::i2s_mclk_multiple_t_I2S_MCLK_MULTIPLE_256;
        clk.dn_sample_mode = sys::i2s_pdm_dsr_t_I2S_PDM_DSR_8S;
        clk.bclk_div = 8;

        let mut slot = sys::i2s_pdm_rx_slot_config_t::default();
        slot.data_bit_width = sys::i2s_data_bit_width_t_I2S_DATA_BIT_WIDTH_16BIT;
        slot.slot_bit_width = sys::i2s_slot_bit_width_t_I2S_SLOT_BIT_WIDTH_AUTO;
        slot.slot_mode = sys::i2s_slot_mode_t_I2S_SLOT_MODE_MONO;
        slot.slot_mask = sys::i2s_pdm_slot_mask_t_I2S_PDM_SLOT_LEFT;
        slot.data_fmt = sys::i2s_pdm_data_fmt_t_I2S_PDM_DATA_FMT_PCM;

        let mut gpio = sys::i2s_pdm_rx_gpio_config_t::default();
        gpio.clk = MIC_CLK_GPIO;
        gpio.__bindgen_anon_1.din = MIC_DIN_GPIO;

        sys::i2s_pdm_rx_config_t {
            clk_cfg: clk,
            slot_cfg: slot,
            gpio_cfg: gpio,
        }
    }

    /// Allocate the PDM RX channel on `I2S_NUM_0` and initialize PDM RX mode.
    ///
    /// The channel is left in the READY state; `esp_codec_dev_open` enables it.
    pub fn pdm_rx_channel() -> Result<sys::i2s_chan_handle_t, Error> {
        let mut chan_cfg = sys::i2s_chan_config_t::default();
        chan_cfg.id = sys::i2s_port_t_I2S_NUM_0;
        chan_cfg.role = sys::i2s_role_t_I2S_ROLE_MASTER;
        chan_cfg.dma_desc_num = 6;
        chan_cfg.dma_frame_num = 240;
        chan_cfg.__bindgen_anon_1.auto_clear = false;
        chan_cfg.auto_clear_before_cb = false;
        chan_cfg.allow_pd = false;
        chan_cfg.intr_priority = 0;

        let mut rx: sys::i2s_chan_handle_t = null_mut();
        let code = unsafe { sys::i2s_new_channel(&chan_cfg, null_mut(), &mut rx) };
        check(code, Step::I2sChannel)?;
        if rx.is_null() {
            return Err(Error::Null { step: Step::I2sChannel });
        }

        let pdm = pdm_config();
        let code = unsafe { sys::i2s_channel_init_pdm_rx_mode(rx, &pdm) };
        if let Err(error) = check(code, Step::I2sPdmMode) {
            unsafe {
                let _ = sys::i2s_del_channel(rx);
            }
            return Err(error);
        }

        Ok(rx)
    }

    /// Build the I2S data interface and the `esp_codec_dev` record handle.
    pub fn codec_device(
        rx: sys::i2s_chan_handle_t,
    ) -> Result<(gmf::esp_codec_dev_handle_t, *const gmf::audio_codec_data_if_t), Error> {
        let mut i2s_cfg = gmf::audio_codec_i2s_cfg_t {
            port: sys::i2s_port_t_I2S_NUM_0 as u8,
            rx_handle: rx as *mut core::ffi::c_void,
            tx_handle: null_mut(),
            clk_src: 0,
        };
        let data_if = unsafe { gmf::audio_codec_new_i2s_data(&mut i2s_cfg) };
        if data_if.is_null() {
            return Err(Error::Null {
                step: Step::I2sDataInterface,
            });
        }

        let mut dev_cfg = gmf::esp_codec_dev_cfg_t {
            dev_type: gmf::esp_codec_dev_type_t_ESP_CODEC_DEV_TYPE_IN,
            codec_if: null(),
            data_if,
        };
        let handle = unsafe { gmf::esp_codec_dev_new(&mut dev_cfg) };
        if handle.is_null() {
            unsafe {
                let _ = gmf::audio_codec_delete_data_if(data_if);
            }
            return Err(Error::Null {
                step: Step::CodecDevice,
            });
        }

        Ok((handle, data_if))
    }

    pub fn audio_source(
        handle: gmf::esp_codec_dev_handle_t,
    ) -> Result<*mut gmf::esp_capture_audio_src_if_t, Error> {
        let mut cfg = gmf::esp_capture_audio_dev_src_cfg_t {
            record_handle: handle,
        };
        let source = unsafe { gmf::esp_capture_new_audio_dev_src(&mut cfg) };
        if source.is_null() {
            return Err(Error::Null {
                step: Step::AudioSource,
            });
        }
        Ok(source)
    }

    pub(crate) fn dvp_config() -> gmf::esp_capture_video_dvp_src_cfg_t {
        gmf::esp_capture_video_dvp_src_cfg_t {
            // The implementation indexes a fixed `pic[2]` array: never exceed 2.
            buf_count: 2,
            pwr_pin: -1,
            reset_pin: -1,
            xclk_pin: 10,
            data: [15, 17, 18, 16, 14, 12, 11, 48],
            vsync_pin: 38,
            href_pin: 47,
            pclk_pin: 13,
            xclk_freq: 10_000_000,
            i2c_port: 0,
        }
    }

    pub fn video_source() -> Result<*mut gmf::esp_capture_video_src_if_t, Error> {
        let mut cfg = dvp_config();
        let source = unsafe { gmf::esp_capture_new_video_dvp_src(&mut cfg) };
        if source.is_null() {
            return Err(Error::Null {
                step: Step::VideoSource,
            });
        }
        Ok(source)
    }

    /// Register the default muxer and encoders. Order matters: encoders first,
    /// then the muxer. All three report success as 0.
    pub fn register_defaults() -> Result<(), Error> {
        let code = unsafe { gmf::esp_audio_enc_register_default() };
        check(code, Step::RegisterAudioEncoder)?;
        let code = unsafe { gmf::esp_video_enc_register_default() };
        check(code, Step::RegisterVideoEncoder)?;
        let code = unsafe { gmf::esp_muxer_register_default() };
        check(code, Step::RegisterMuxer)
    }

    pub fn capture_service(
        cfg: &gmf::esp_capture_service_cfg_t,
    ) -> Result<*mut gmf::esp_capture_service_t, Error> {
        let mut service: *mut gmf::esp_capture_service_t = null_mut();
        let code = unsafe { gmf::esp_capture_service_create(cfg, &mut service) };
        check(code, Step::CaptureService)?;
        if service.is_null() {
            return Err(Error::Null {
                step: Step::CaptureService,
            });
        }
        Ok(service)
    }

    /// Fill the setup bundle (sources, tracks, muxer), apply it, destroy it.
    pub fn configure_capture(
        service: *mut gmf::esp_capture_service_t,
        service_cfg: &gmf::esp_capture_service_cfg_t,
        audio_src: *mut gmf::esp_capture_audio_src_if_t,
        video_src: *mut gmf::esp_capture_video_src_if_t,
        cfg: &Config<'_>,
    ) -> Result<(), Error> {
        let setup = unsafe { gmf::esp_capture_service_setup_create(service_cfg) };
        if setup.is_null() {
            return Err(Error::Null {
                step: Step::CaptureSetup,
            });
        }

        let result = configure_setup(service, setup, audio_src, video_src, cfg);
        let destroy_code = unsafe { gmf::esp_capture_service_setup_destroy(setup) };
        result?;
        check(destroy_code, Step::CaptureSetupDestroy)
    }

    fn configure_setup(
        service: *mut gmf::esp_capture_service_t,
        setup: *mut gmf::esp_capture_service_setup_t,
        audio_src: *mut gmf::esp_capture_audio_src_if_t,
        video_src: *mut gmf::esp_capture_video_src_if_t,
        cfg: &Config<'_>,
    ) -> Result<(), Error> {
        let src_cfg = gmf::esp_capture_service_src_cfg_t {
            audio_src,
            video_src,
            share_overlay: false,
            full_speed_decode: false,
        };
        let code = unsafe { gmf::esp_capture_service_setup_src(setup, &src_cfg) };
        check(code, Step::CaptureSourceConfig)?;

        let video_track = video_track_info(cfg);
        let code =
            unsafe { gmf::esp_capture_service_setup_add_track(setup, DEFAULT_STREAM, &video_track) };
        check(code, Step::CaptureTrackVideo)?;

        let audio_track = audio_track_info();
        let code =
            unsafe { gmf::esp_capture_service_setup_add_track(setup, DEFAULT_STREAM, &audio_track) };
        check(code, Step::CaptureTrackAudio)?;

        let code = unsafe { gmf::esp_capture_service_setup_apply(service, setup) };
        check(code, Step::CaptureSetupApply)
    }

    pub fn muxer_service(cfg: &Config<'_>) -> Result<*mut gmf::esp_muxer_service_t, Error> {
        let mut service: *mut gmf::esp_muxer_service_t = null_mut();
        let service_cfg = gmf::esp_muxer_service_cfg_t {
            name: MUXER_SERVICE_NAME.as_ptr() as *const c_char,
        };
        let code = unsafe { gmf::esp_muxer_service_create(&service_cfg, &mut service) };
        check(code, Step::MuxerService)?;
        if service.is_null() {
            return Err(Error::Null {
                step: Step::MuxerService,
            });
        }

        let dir = format!("{}/{}", cfg.mount_point.trim_end_matches('/'), RECORD_DIR);
        let storage = CString::new(dir).map_err(|_| Error::Config {
            step: Step::MuxerSetup,
        })?;
        let setup = gmf::esp_muxer_service_setup_t {
            muxer_type: gmf::esp_muxer_type_t_ESP_MUXER_TYPE_AVI,
            mode: gmf::esp_muxer_service_mode_t_ESP_MUXER_SERVICE_MODE_STORAGE_ONLY,
            storage_dir: storage.as_ptr() as *const c_char,
            slice_duration: 0,
            ram_cache_size: MUXER_RAM_CACHE,
            streaming_cache_size: 0,
        };
        let code = unsafe { gmf::esp_muxer_service_setup(service, &setup) };
        if let Err(error) = check(code, Step::MuxerSetup) {
            unsafe {
                let _ = gmf::esp_media_service_deinit(service as *mut gmf::esp_service_t);
                sys::free(service as *mut core::ffi::c_void);
            }
            return Err(error);
        }
        Ok(service)
    }

    pub fn link_muxer(
        capture: *mut gmf::esp_capture_service_t,
        muxer: *mut gmf::esp_muxer_service_t,
    ) -> Result<(), Error> {
        let code = unsafe {
            gmf::esp_media_service_link(
                capture as *mut gmf::esp_service_t,
                DEFAULT_STREAM,
                muxer as *mut gmf::esp_service_t,
                DEFAULT_STREAM,
            )
        };
        check(code, Step::MuxerLink)
    }

    pub fn start_muxer(service: *mut gmf::esp_muxer_service_t) -> Result<(), Error> {
        let code = unsafe { gmf::esp_service_start(service as *mut gmf::esp_service_t) };
        check(code, Step::MuxerStart)
    }

    pub fn rtsp_service(
        cfg: &gmf::esp_rtsp_service_cfg_t,
        pipeline_cfg: &Config<'_>,
    ) -> Result<*mut gmf::esp_rtsp_service_t, Error> {
        let mut service: *mut gmf::esp_rtsp_service_t = null_mut();
        let code = unsafe { gmf::esp_rtsp_service_create(cfg, &mut service) };
        check(code, Step::RtspService)?;
        if service.is_null() {
            return Err(Error::Null {
                step: Step::RtspService,
            });
        }

        let result = configure_rtsp(service, pipeline_cfg);
        if let Err(error) = result {
            unsafe {
                let _ = gmf::esp_media_service_deinit(service as *mut gmf::esp_service_t);
                sys::free(service as *mut core::ffi::c_void);
            }
            return Err(error);
        }

        Ok(service)
    }

    fn configure_rtsp(
        service: *mut gmf::esp_rtsp_service_t,
        pipeline_cfg: &Config<'_>,
    ) -> Result<(), Error> {
        // The RTSP core buffers one encoded frame per track. The 64 KiB
        // default is smaller than a single 1080p JPEG (hundreds of KB), and
        // oversized frames are dropped silently, so size it from the geometry.
        let vid_frame_size = (pipeline_cfg.width * pipeline_cfg.height / 4)
            .clamp(64 * 1024, 1024 * 1024) as u32;
        let setup = gmf::esp_rtsp_service_setup_t {
            local_port: pipeline_cfg.port,
            transport: gmf::esp_rtsp_transport_t_RTSP_TRANSPORT_UDP,
            audio_enable: true,
            video_enable: true,
            audio_cache_size: 0,
            video_cache_size: 0,
            aud_frame_size: 0,
            vid_frame_size,
        };
        let code = unsafe { gmf::esp_rtsp_service_setup(service, &setup) };
        check(code, Step::RtspSetup)?;

        let url = CString::new(format!(
            "rtsp://0.0.0.0:{}{}",
            pipeline_cfg.port, RTSP_PATH
        ))
        .map_err(|_| Error::Config {
            step: Step::RtspUrl,
        })?;
        let code = unsafe { gmf::esp_rtsp_service_set_url(service, url.as_ptr()) };
        check(code, Step::RtspUrl)?;

        if let Some(ipv4) = pipeline_cfg.ipv4.filter(|ip| !ip.is_empty()) {
            let ip = CString::new(ipv4).map_err(|_| Error::Config { step: Step::RtspIp })?;
            let code = unsafe { gmf::esp_rtsp_service_set_ip(service, ip.as_ptr()) };
            check(code, Step::RtspIp)?;
        }

        Ok(())
    }

    pub fn link(
        capture: *mut gmf::esp_capture_service_t,
        rtsp: *mut gmf::esp_rtsp_service_t,
    ) -> Result<(), Error> {
        let code = unsafe {
            gmf::esp_media_service_link(
                capture as *mut gmf::esp_service_t,
                DEFAULT_STREAM,
                rtsp as *mut gmf::esp_service_t,
                DEFAULT_STREAM,
            )
        };
        check(code, Step::MediaLink)
    }

    fn unlink(
        capture: *mut gmf::esp_capture_service_t,
        rtsp: *mut gmf::esp_rtsp_service_t,
    ) -> Result<(), Error> {
        let code = unsafe {
            gmf::esp_media_service_unlink(
                capture as *mut gmf::esp_service_t,
                DEFAULT_STREAM,
                rtsp as *mut gmf::esp_service_t,
                DEFAULT_STREAM,
            )
        };
        check(code, Step::MediaLink)
    }

    pub fn start_rtsp(service: *mut gmf::esp_rtsp_service_t) -> Result<(), Error> {
        let code = unsafe { gmf::esp_service_start(service as *mut gmf::esp_service_t) };
        check(code, Step::RtspStart)
    }

    pub fn start_capture(service: *mut gmf::esp_capture_service_t) -> Result<(), Error> {
        let code = unsafe { gmf::esp_service_start(service as *mut gmf::esp_service_t) };
        check(code, Step::CaptureStart)
    }

    /// Stop then release every handle, in reverse creation order.
    pub fn stop(pipeline: &mut Pipeline) {
        logging::log!("pipeline: stop");
        unsafe {
            // Reverse start order: RTSP stops before capture so the consumer
            // is not still querying a stopped provider. If the start thread may
            // still be inside `esp_service_start`, stopping could deadlock, so
            // skip it and let deinit/free be best-effort.
            if pipeline.rtsp_started && !pipeline.rtsp_start_pending {
                let _ = gmf::esp_service_stop(pipeline.rtsp as *mut gmf::esp_service_t);
                pipeline.rtsp_started = false;
            }
            if pipeline.capture_started {
                let _ = gmf::esp_service_stop(pipeline.capture as *mut gmf::esp_service_t);
                pipeline.capture_started = false;
            }
            if pipeline.muxer_started {
                let _ = gmf::esp_service_stop(pipeline.muxer as *mut gmf::esp_service_t);
                pipeline.muxer_started = false;
            }
            if pipeline.linked {
                let _ = unlink(pipeline.capture, pipeline.rtsp);
                pipeline.linked = false;
            }
            if pipeline.muxer_linked {
                let _ = gmf::esp_media_service_unlink(
                    pipeline.capture as *mut gmf::esp_service_t,
                    DEFAULT_STREAM,
                    pipeline.muxer as *mut gmf::esp_service_t,
                    DEFAULT_STREAM,
                );
                pipeline.muxer_linked = false;
            }
            if !pipeline.muxer.is_null() {
                let _ = gmf::esp_media_service_deinit(pipeline.muxer as *mut gmf::esp_service_t);
                sys::free(pipeline.muxer as *mut core::ffi::c_void);
                pipeline.muxer = null_mut();
            }
            if !pipeline.rtsp.is_null() {
                let _ = gmf::esp_media_service_deinit(pipeline.rtsp as *mut gmf::esp_service_t);
                sys::free(pipeline.rtsp as *mut core::ffi::c_void);
                pipeline.rtsp = null_mut();
            }
            if !pipeline.capture.is_null() {
                let _ = gmf::esp_capture_service_destroy(pipeline.capture);
                pipeline.capture = null_mut();
            }
            if pipeline.registered {
                gmf::esp_muxer_unregister_default();
                gmf::esp_video_enc_unregister_default();
                gmf::esp_audio_enc_unregister_default();
                pipeline.registered = false;
            }
            if !pipeline.dev_handle.is_null() {
                gmf::esp_codec_dev_delete(pipeline.dev_handle);
                pipeline.dev_handle = null_mut();
            }
            if !pipeline.data_if.is_null() {
                let _ = gmf::audio_codec_delete_data_if(pipeline.data_if);
                pipeline.data_if = null();
            }
            if !pipeline.i2s_rx.is_null() {
                let _ = sys::i2s_channel_disable(pipeline.i2s_rx);
                let _ = sys::i2s_del_channel(pipeline.i2s_rx);
                pipeline.i2s_rx = null_mut();
            }
            if !pipeline.i2c_bus.is_null() {
                let _ = sys::i2c_del_master_bus(pipeline.i2c_bus);
                pipeline.i2c_bus = null_mut();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_steps_render_snake_case_names() {
        assert_eq!(Step::MediaLibAdapter.as_str(), "media_lib_adapter");
        assert_eq!(Step::Scheduler.as_str(), "scheduler");
        assert_eq!(
            Step::RegisterAudioEncoder.as_str(),
            "register_audio_encoder"
        );
        assert_eq!(
            Step::RegisterVideoEncoder.as_str(),
            "register_video_encoder"
        );
        assert_eq!(Step::RegisterMuxer.as_str(), "register_muxer");
    }

    #[test]
    fn check_maps_esp_ok_and_failure() {
        assert!(ffi::check(sys::ESP_OK, Step::I2cBus).is_ok());
        let error = ffi::check(-1, Step::RtspStart).expect_err("non-zero must fail");
        assert_eq!(
            error,
            Error::Esp {
                step: Step::RtspStart,
                code: -1
            }
        );
    }

    #[test]
    fn error_display_names_the_failed_step() {
        let esp = Error::Esp {
            step: Step::CaptureSetupApply,
            code: 258,
        };
        assert!(esp.to_string().contains("capture_setup_apply"));
        assert!(esp.to_string().contains("258"));

        let null = Error::Null {
            step: Step::VideoSource,
        };
        assert!(null.to_string().contains("video_source"));

        let config = Error::Config {
            step: Step::RtspUrl,
        };
        assert!(config.to_string().contains("rtsp_url"));
    }

    #[test]
    fn scheduler_overrides_rtsp_threads_and_keeps_unknown_defaults() {
        assert_eq!(thread_override("rtsp_server"), Some((8192, 1, 1)));
        assert_eq!(thread_override("rtsp_push"), Some((8192, 1, 1)));
        assert_eq!(thread_override("rtsp_src"), Some((8192, 1, 1)));
        assert_eq!(thread_override("video_capture"), Some((8192, 8, 0)));
        assert_eq!(thread_override("VID_SRC"), Some((8192, 8, 0)));
        assert_eq!(thread_override("muxer_sink"), Some((8192, 5, 1)));
        // Unknown threads get `None`, which the callback maps to ESP_ERR_NOT_FOUND.
        assert_eq!(thread_override("mystery_thread"), None);
    }

    #[test]
    fn video_track_uses_the_configured_scale() {
        let cfg = Config {
            ipv4: None,
            mount_point: "",
            port: 554,
            width: 640,
            height: 480,
            fps: 15,
        };
        let track = video_track_info(&cfg);
        let video = unsafe { track.info.video };
        assert_eq!(video.width, 640);
        assert_eq!(video.height, 480);
        assert_eq!(video.fps, 15);
        assert_eq!(
            video.codec,
            gmf::esp_capture_format_id_t_ESP_CAPTURE_FMT_ID_MJPEG as u32
        );
    }

    #[test]
    fn audio_track_matches_the_static_g711_clock() {
        let track = audio_track_info();
        let audio = unsafe { track.info.audio };
        assert_eq!(
            audio.codec,
            gmf::esp_capture_format_id_t_ESP_CAPTURE_FMT_ID_G711U as u32
        );
        // PT 0 is PCMU/8000 in RFC 3551; a different rate doubles the
        // playback duration on every client.
        assert_eq!(audio.sample_rate, 8_000);
        assert_eq!(audio.channel, 1);
        assert_eq!(audio.bits_per_sample, 16);
    }

    #[test]
    fn config_validation_rejects_bad_geometry() {
        let base = |width, height, fps| Config {
            ipv4: None,
            mount_point: "",
            port: 554,
            width,
            height,
            fps,
        };
        assert!(base(320, 240, 10).validate().is_ok());
        assert!(base(640, 480, 10).validate().is_ok());
        assert!(matches!(
            base(0, 240, 10).validate(),
            Err(Error::InvalidConfig(_))
        ));
        assert!(matches!(
            base(321, 240, 10).validate(),
            Err(Error::InvalidConfig(_))
        ));
        assert!(matches!(
            base(320, 240, 0).validate(),
            Err(Error::InvalidConfig(_))
        ));
        assert!(matches!(
            base(320, 240, 31).validate(),
            Err(Error::InvalidConfig(_))
        ));
    }

    #[test]
    fn dvp_source_honours_the_hardware_constraints() {
        let cfg = ffi::dvp_config();
        assert_eq!(cfg.buf_count, 2, "implementation indexes a fixed pic[2] array");
        assert_eq!(cfg.xclk_pin, 10);
        assert_eq!(cfg.vsync_pin, 38);
        assert_eq!(cfg.href_pin, 47);
        assert_eq!(cfg.pclk_pin, 13);
        assert_eq!(cfg.data, [15, 17, 18, 16, 14, 12, 11, 48]);
        assert_eq!(cfg.xclk_freq, 10_000_000);
        assert_eq!(cfg.i2c_port, 0);
        assert_eq!((cfg.pwr_pin, cfg.reset_pin), (-1, -1));
    }

    #[test]
    fn pdm_source_is_16khz_mono_pcm_on_the_board_pins() {
        let cfg = ffi::pdm_config();
        assert_eq!(cfg.clk_cfg.sample_rate_hz, 16_000);
        assert_eq!(cfg.clk_cfg.bclk_div, 8);
        assert_eq!(
            cfg.slot_cfg.data_bit_width,
            sys::i2s_data_bit_width_t_I2S_DATA_BIT_WIDTH_16BIT
        );
        assert_eq!(
            cfg.slot_cfg.slot_mode,
            sys::i2s_slot_mode_t_I2S_SLOT_MODE_MONO
        );
        assert_eq!(
            cfg.slot_cfg.data_fmt,
            sys::i2s_pdm_data_fmt_t_I2S_PDM_DATA_FMT_PCM
        );
        assert_eq!(cfg.gpio_cfg.clk, MIC_CLK_GPIO);
        assert_eq!(unsafe { cfg.gpio_cfg.__bindgen_anon_1.din }, MIC_DIN_GPIO);
    }

    #[test]
    fn i2c_bus_uses_sccb_pins_on_port_zero() {
        let cfg = ffi::i2c_config();
        assert_eq!(cfg.i2c_port, 0);
        assert_eq!(cfg.sda_io_num, sys::gpio_num_t_GPIO_NUM_40);
        assert_eq!(cfg.scl_io_num, sys::gpio_num_t_GPIO_NUM_39);
        assert_eq!(cfg.flags.enable_internal_pullup(), 1);
    }
}
