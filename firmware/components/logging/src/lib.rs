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
            c"app".as_ptr(),
            c"%s".as_ptr(),
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
