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
/// RTSP path shared by the server URL.
const RTSP_PATH: &str = "/live";
/// Default stream id (`ESP_MEDIA_DEFAULT_STREAM`).
const DEFAULT_STREAM: u16 = 0;
/// Directory created under the mount point for AVI recordings.
const RECORD_DIR: &str = "rec";
/// PDM microphone clock pin.
const MIC_CLK_GPIO: i32 = 42;
/// PDM microphone data pin.
const MIC_DIN_GPIO: i32 = 41;
/// Camera SCCB SDA pin.
const SCCB_SDA_GPIO: i32 = 40;
/// Camera SCCB SCL pin.
const SCCB_SCL_GPIO: i32 = 39;
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
        }
    }
}

impl std::error::Error for Error {}

/// Owns every handle of the running media pipeline.
pub struct Pipeline {
    rtsp: *mut gmf::esp_rtsp_service_t,
    capture: *mut gmf::esp_capture_service_t,
    dev_handle: gmf::esp_codec_dev_handle_t,
    data_if: *const gmf::audio_codec_data_if_t,
    i2s_rx: sys::i2s_chan_handle_t,
    i2c_bus: sys::i2c_master_bus_handle_t,
    linked: bool,
    rtsp_started: bool,
    capture_started: bool,
    recording: bool,
    registered: bool,
}

impl Pipeline {
    /// Build and start the full pipeline.
    pub fn start(cfg: &Config<'_>) -> Result<Self, Error> {
        let mut pipeline = Pipeline {
            rtsp: std::ptr::null_mut(),
            capture: std::ptr::null_mut(),
            dev_handle: std::ptr::null_mut(),
            data_if: std::ptr::null(),
            i2s_rx: std::ptr::null_mut(),
            i2c_bus: std::ptr::null_mut(),
            linked: false,
            rtsp_started: false,
            capture_started: false,
            recording: !cfg.mount_point.is_empty(),
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
        self.recording
    }

    /// Whether the audio source (PDM microphone + codec device) is present.
    pub fn audio(&self) -> bool {
        !self.dev_handle.is_null()
    }

    /// Whether the RTSP server is running.
    pub fn rtsp(&self) -> bool {
        self.rtsp_started
    }

    fn start_inner(&mut self, cfg: &Config<'_>) -> Result<(), Error> {
        // 1. Camera SCCB bus: esp32-camera's sccb-ng uses the *new* I2C master
        //    driver, so the bus must exist before esp_capture_start.
        self.i2c_bus = ffi::i2c_master_bus()?;

        // 2. PDM microphone I2S RX channel, initialized but deliberately NOT
        //    enabled: esp_codec_dev_open() enables it exactly once via the
        //    I2S data interface's set_fmt/enable path.
        self.i2s_rx = ffi::pdm_rx_channel()?;

        // 3. esp_codec_dev record device backed by the I2S data interface.
        let (dev_handle, data_if) = ffi::codec_device(self.i2s_rx)?;
        self.dev_handle = dev_handle;
        self.data_if = data_if;

        // 4. Audio source (PCM; G711U is encoded downstream by the capture
        //    service's GMF audio path).
        let audio_src = ffi::audio_source(self.dev_handle)?;

        // 5. DVP video source (OV3660, MJPEG).
        let video_src = ffi::video_source()?;

        // 6. Register the default muxer and encoders. esp_capture resolves
        //    them through a registry (esp_muxer_open, encoder lookups), so
        //    they must be present before the capture service is created.
        ffi::register_defaults()?;
        self.registered = true;

        // 7. Capture service.
        let service_cfg = gmf::esp_capture_service_cfg_t {
            name: CAPTURE_SERVICE_NAME.as_ptr() as *const core::ffi::c_char,
            max_stream_num: 1,
        };
        self.capture = ffi::capture_service(&service_cfg)?;

        // 8. Setup bundle: sources, tracks, muxer; apply and destroy.
        ffi::configure_capture(self.capture, &service_cfg, audio_src, video_src, cfg)?;

        // 9. RTSP service.
        let rtsp_cfg = gmf::esp_rtsp_service_cfg_t {
            name: RTSP_SERVICE_NAME.as_ptr() as *const core::ffi::c_char,
            role: gmf::esp_rtsp_service_role_t_ESP_RTSP_SERVICE_ROLE_SERVER,
        };
        self.rtsp = ffi::rtsp_service(&rtsp_cfg, cfg)?;

        // 10. Join capture -> RTSP over the default stream, no frame plumbing.
        ffi::link(self.capture, self.rtsp)?;
        self.linked = true;

        // 11. Contractual start order: RTSP first, then capture.
        ffi::start_rtsp(self.rtsp)?;
        self.rtsp_started = true;
        ffi::start_capture(self.capture)?;
        self.capture_started = true;

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
    use core::ptr::{null, null_mut};

    /// Convert an `esp_err_t` into a step-named result.
    fn check(code: i32, step: Step) -> Result<(), Error> {
        if code == sys::ESP_OK {
            Ok(())
        } else {
            Err(Error::Esp { step, code })
        }
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

    fn i2c_config() -> sys::i2c_master_bus_config_t {
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

    fn pdm_config() -> sys::i2s_pdm_rx_config_t {
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

    fn dvp_config() -> gmf::esp_capture_video_dvp_src_cfg_t {
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
            xclk_freq: 20_000_000,
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

        let video_track = gmf::esp_media_track_info_t {
            id: 1,
            type_: gmf::esp_media_track_type_t_ESP_MEDIA_TRACK_TYPE_VIDEO,
            info: gmf::esp_media_track_info_t__bindgen_ty_1 {
                video: gmf::esp_media_video_info_t {
                    codec: gmf::esp_capture_format_id_t_ESP_CAPTURE_FMT_ID_MJPEG as u32,
                    width: 320,
                    height: 240,
                    fps: 10,
                    bitrate: 0,
                },
            },
        };
        let code =
            unsafe { gmf::esp_capture_service_setup_add_track(setup, DEFAULT_STREAM, &video_track) };
        check(code, Step::CaptureTrackVideo)?;

        let audio_track = gmf::esp_media_track_info_t {
            id: 2,
            type_: gmf::esp_media_track_type_t_ESP_MEDIA_TRACK_TYPE_AUDIO,
            info: gmf::esp_media_track_info_t__bindgen_ty_1 {
                audio: gmf::esp_media_audio_info_t {
                    codec: gmf::esp_capture_format_id_t_ESP_CAPTURE_FMT_ID_G711U as u32,
                    sample_rate: 16_000,
                    bits_per_sample: 16,
                    channel: 1,
                    bitrate: 64_000,
                },
            },
        };
        let code =
            unsafe { gmf::esp_capture_service_setup_add_track(setup, DEFAULT_STREAM, &audio_track) };
        check(code, Step::CaptureTrackAudio)?;

        // Storage muxer. Without a mounted card, disable file storage but keep
        // the capture/stream path alive.
        let storage = if cfg.mount_point.is_empty() {
            None
        } else {
            let dir = format!("{}/{}", cfg.mount_point.trim_end_matches('/'), RECORD_DIR);
            Some(CString::new(dir).map_err(|_| Error::Config {
                step: Step::CaptureMuxerConfig,
            })?)
        };
        let muxer = gmf::esp_capture_service_muxer_cfg_t {
            muxer_type: gmf::esp_muxer_type_t_ESP_MUXER_TYPE_AVI,
            auto_record: storage.is_some(),
            streaming: false,
            storage_dir: storage
                .as_ref()
                .map_or(null(), |dir| dir.as_ptr() as *const c_char),
            slice_duration: 0,
            ram_cache_size: MUXER_RAM_CACHE,
        };
        let code = unsafe {
            gmf::esp_capture_service_setup_set_muxer_cfg(setup, DEFAULT_STREAM, &muxer)
        };
        check(code, Step::CaptureMuxerConfig)?;

        let code = unsafe { gmf::esp_capture_service_setup_apply(service, setup) };
        check(code, Step::CaptureSetupApply)
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
        let setup = gmf::esp_rtsp_service_setup_t {
            local_port: pipeline_cfg.port,
            transport: gmf::esp_rtsp_transport_t_RTSP_TRANSPORT_UDP,
            audio_enable: true,
            video_enable: true,
            audio_cache_size: 0,
            video_cache_size: 0,
            aud_frame_size: 0,
            vid_frame_size: 0,
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
        unsafe {
            if pipeline.capture_started {
                let _ = gmf::esp_service_stop(pipeline.capture as *mut gmf::esp_service_t);
                pipeline.capture_started = false;
            }
            if pipeline.rtsp_started {
                let _ = gmf::esp_service_stop(pipeline.rtsp as *mut gmf::esp_service_t);
                pipeline.rtsp_started = false;
            }
            if pipeline.linked {
                let _ = unlink(pipeline.capture, pipeline.rtsp);
                pipeline.linked = false;
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
    fn dvp_source_honours_the_hardware_constraints() {
        let cfg = ffi::dvp_config();
        assert_eq!(cfg.buf_count, 2, "implementation indexes a fixed pic[2] array");
        assert_eq!(cfg.xclk_pin, 10);
        assert_eq!(cfg.vsync_pin, 38);
        assert_eq!(cfg.href_pin, 47);
        assert_eq!(cfg.pclk_pin, 13);
        assert_eq!(cfg.data, [15, 17, 18, 16, 14, 12, 11, 48]);
        assert_eq!(cfg.xclk_freq, 20_000_000);
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
        assert_eq!(cfg.gpio_cfg.__bindgen_anon_1.din, MIC_DIN_GPIO);
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
