use core::ffi::c_char;
use std::ffi::CString;
use std::sync::Mutex;

static LOG_LOCK: Mutex<()> = Mutex::new(());

pub fn print(arguments: std::fmt::Arguments<'_>) {
    let text = format!("{arguments}");
    let text = if text.ends_with('\n') {
        text
    } else {
        format!("{text}\n")
    };
    let Ok(message) = CString::new(text) else {
        return;
    };
    let _guard = LOG_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    unsafe {
        esp_idf_sys::esp_rom_printf(
            b"%s\0".as_ptr() as *const c_char,
            message.as_ptr(),
        );
    }
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        $crate::logging::print(format_args!($($arg)*))
    };
}
