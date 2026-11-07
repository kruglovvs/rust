//! Device entry point.
//!
//! Subsystem modules and their unit/HIL tests live together in this binary's
//! `src/*.rs` files. Device-only HIL tests are gated with `target_os = "espidf"`.
//!
#[allow(dead_code)]
mod artifact_server;
#[allow(dead_code)]
mod audio_quality;
#[allow(dead_code)]
mod camera;
#[cfg(test)]
#[allow(dead_code)]
mod device_boundary_tests;
#[allow(dead_code)]
mod jfif;
#[allow(dead_code)]
mod mic;
#[allow(dead_code)]
mod muxer;
#[allow(dead_code)]
mod net;
#[allow(dead_code)]
mod recorder;
#[allow(dead_code)]
mod rtcp;
#[allow(dead_code)]
mod rtsp;
mod logging;
mod runtime;
#[allow(dead_code)]
mod sensor;
#[allow(dead_code)]
mod session;
#[allow(dead_code)]
mod sd_probe;
#[allow(dead_code)]
mod storage;
#[allow(dead_code)]
#[path = "rtsp/payload/mod.rs"]
mod payload;

#[cfg(test)]
pub(crate) static HIL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) mod test_support {
    use core::ffi::c_char;
    use std::ffi::CString;
    use std::fmt;

    pub fn rom_print(arguments: fmt::Arguments<'_>) {
        let message = CString::new(format!("{arguments}"))
            .expect("test log message must not contain an interior NUL");
        unsafe {
            esp_idf_sys::esp_rom_printf(
                b"%s\0".as_ptr() as *const c_char,
                message.as_ptr(),
            );
        }
    }
}

const RUNTIME_STACK_BYTES: usize = 64 * 1024;

fn main() {
    let spawned = std::thread::Builder::new()
        .name("runtime".into())
        .stack_size(RUNTIME_STACK_BYTES)
        .spawn(runtime::run);

    match spawned {
        Ok(handle) => {
            let _ = handle.join();
        }
        Err(error) => {
            println!("boot: FATAL: cannot spawn runtime thread: {error}");
            loop {
                std::thread::sleep(std::time::Duration::from_secs(60));
            }
        }
    }
}
