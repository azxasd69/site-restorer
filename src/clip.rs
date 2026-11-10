//! Буфер обмена: чтение и запись текста без сторонних крейтов.
//!
//! На Windows используется WinAPI напрямую: в связке egui-winit+arboard
//! чтение буфера, которым владеет другой процесс, иногда возвращает пустоту —
//! окно при этом получает Ctrl+V, но текст не вставляется.

/// Текст из буфера обмена. `None` — буфер пуст или недоступен.
#[cfg(windows)]
pub fn read_text() -> Option<String> {
    use std::ffi::c_void;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn OpenClipboard(owner: *mut c_void) -> i32;
        fn CloseClipboard() -> i32;
        fn GetClipboardData(format: u32) -> *mut c_void;
        fn IsClipboardFormatAvailable(format: u32) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalLock(mem: *mut c_void) -> *mut c_void;
        fn GlobalUnlock(mem: *mut c_void) -> i32;
        fn GlobalSize(mem: *mut c_void) -> usize;
    }

    const CF_UNICODETEXT: u32 = 13;

    // SAFETY: буфер обмена открывается на время чтения, хендл закрывается в
    // любом случае; данные копируются до GlobalUnlock.
    unsafe {
        if IsClipboardFormatAvailable(CF_UNICODETEXT) == 0 {
            return None;
        }
        // Буфер может быть занят другим процессом — пробуем несколько раз.
        let mut opened = 0;
        for _ in 0..8 {
            opened = OpenClipboard(std::ptr::null_mut());
            if opened != 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        if opened == 0 {
            return None;
        }
        let handle = GetClipboardData(CF_UNICODETEXT);
        if handle.is_null() {
            CloseClipboard();
            return None;
        }
        let ptr = GlobalLock(handle) as *const u16;
        if ptr.is_null() {
            CloseClipboard();
            return None;
        }
        let bytes = GlobalSize(handle);
        let units = bytes / 2;
        let mut len = 0;
        while len < units && *ptr.add(len) != 0 {
            len += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
        GlobalUnlock(handle);
        CloseClipboard();
        if text.is_empty() { None } else { Some(text) }
    }
}

#[cfg(not(windows))]
pub fn read_text() -> Option<String> {
    None
}

/// Записать текст в буфер обмена.
#[cfg(windows)]
pub fn write_text(text: &str) {
    use std::ffi::c_void;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn OpenClipboard(owner: *mut c_void) -> i32;
        fn CloseClipboard() -> i32;
        fn EmptyClipboard() -> i32;
        fn SetClipboardData(format: u32, mem: *mut c_void) -> *mut c_void;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalAlloc(flags: u32, bytes: usize) -> *mut c_void;
        fn GlobalLock(mem: *mut c_void) -> *mut c_void;
        fn GlobalUnlock(mem: *mut c_void) -> i32;
        fn GlobalFree(mem: *mut c_void) -> *mut c_void;
    }

    const CF_UNICODETEXT: u32 = 13;
    const GMEM_MOVEABLE: u32 = 0x0002;

    let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = units.len() * 2;
    // SAFETY: память выделяется и освобождается в одном месте; при успешном
    // SetClipboardData владельцем становится система, и GlobalFree не нужен.
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return;
        }
        if EmptyClipboard() == 0 {
            CloseClipboard();
            return;
        }
        let mem = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if mem.is_null() {
            CloseClipboard();
            return;
        }
        let dst = GlobalLock(mem) as *mut u8;
        if dst.is_null() {
            GlobalFree(mem);
            CloseClipboard();
            return;
        }
        std::ptr::copy_nonoverlapping(units.as_ptr() as *const u8, dst, bytes);
        GlobalUnlock(mem);
        if SetClipboardData(CF_UNICODETEXT, mem).is_null() {
            GlobalFree(mem);
        }
        CloseClipboard();
    }
}

#[cfg(not(windows))]
pub fn write_text(_text: &str) {}
