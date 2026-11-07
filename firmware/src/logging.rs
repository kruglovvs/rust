use std::ffi::CString;
use std::sync::Mutex;

static LOG_LOCK: Mutex<()> = Mutex::new(());

mod ffi {
    use core::ffi::c_char;
    use std::ffi::CString;

    /// `esp_rom_printf` is ROM-resident and bypasses VFS, which is why thread
    /// logging uses it: `println!` from a non-main thread writes through the VFS
    /// path and crashes on this target. It is not reentrant, so callers serialize.
    pub(super) fn rom_printf(message: &CString) {
        unsafe {
            esp_idf_sys::esp_rom_printf(
                b"%s\0".as_ptr() as *const c_char,
                message.as_ptr(),
            );
        }
    }
}

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
    ffi::rom_printf(&message);
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        $crate::logging::print(format_args!($($arg)*))
    };
}
