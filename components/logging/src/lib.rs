use std::ffi::CString;

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
    // SAFETY: both C strings are NUL-terminated and live for the call;
    // esp_log_write is internally synchronized.
    unsafe {
        esp_idf_sys::esp_log_write(
            esp_idf_sys::esp_log_level_t_ESP_LOG_INFO,
            b"app\0".as_ptr() as *const core::ffi::c_char,
            b"%s\0".as_ptr() as *const core::ffi::c_char,
            message.as_ptr(),
        );
    }
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        $crate::print(format_args!($($arg)*))
    };
}
