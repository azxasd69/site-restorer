//! Выбор папки системным диалогом без сторонних крейтов.
//!
//! На Windows используется `SHBrowseForFolderW` из shell32 — он есть в любой
//! системе и не требует COM-обвязки сверх `CoInitializeEx`. На остальных
//! платформах кнопка «Обзор…» не показывается: путь вводится в поле вручную.

/// Открыть диалог выбора папки. `None` — пользователь отменил или платформа
/// не поддерживает диалог.
#[cfg(windows)]
pub fn pick_folder() -> Option<String> {
    use std::ffi::c_void;

    #[repr(C)]
    struct BrowseInfoW {
        hwnd_owner: *mut c_void,
        pidl_root: *mut c_void,
        psz_display_name: *mut u16,
        lpsz_title: *const u16,
        ul_flags: u32,
        lpfn: *mut c_void,
        l_param: isize,
        i_image: i32,
    }

    #[link(name = "shell32")]
    unsafe extern "system" {
        fn SHBrowseForFolderW(info: *mut BrowseInfoW) -> *mut c_void;
        fn SHGetPathFromIDListW(pidl: *const c_void, path: *mut u16) -> i32;
    }
    #[link(name = "ole32")]
    unsafe extern "system" {
        fn CoInitializeEx(reserved: *mut c_void, flags: u32) -> i32;
        fn CoTaskMemFree(ptr: *mut c_void);
    }

    const BIF_RETURNONLYFSDIRS: u32 = 0x0001;
    const BIF_EDITBOX: u32 = 0x0010;
    const BIF_NEWDIALOGSTYLE: u32 = 0x0040;
    const COINIT_APARTMENTTHREADED: u32 = 0x2;

    let title: Vec<u16> = "Выберите папку для копии\0".encode_utf16().collect();
    let mut buffer = [0u16; 260];
    // SAFETY: структура и буфер живут до конца вызова; PIDL освобождается
    // через CoTaskMemFree, как требует документация.
    unsafe {
        let _ = CoInitializeEx(std::ptr::null_mut(), COINIT_APARTMENTTHREADED);
        let mut info = BrowseInfoW {
            hwnd_owner: std::ptr::null_mut(),
            pidl_root: std::ptr::null_mut(),
            psz_display_name: std::ptr::null_mut(),
            lpsz_title: title.as_ptr(),
            ul_flags: BIF_RETURNONLYFSDIRS | BIF_EDITBOX | BIF_NEWDIALOGSTYLE,
            lpfn: std::ptr::null_mut(),
            l_param: 0,
            i_image: 0,
        };
        let pidl = SHBrowseForFolderW(&mut info);
        if pidl.is_null() {
            return None;
        }
        let ok = SHGetPathFromIDListW(pidl, buffer.as_mut_ptr());
        CoTaskMemFree(pidl);
        if ok == 0 {
            return None;
        }
    }
    let len = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
    let path = String::from_utf16_lossy(&buffer[..len]);
    if path.is_empty() { None } else { Some(path) }
}

#[cfg(not(windows))]
pub fn pick_folder() -> Option<String> {
    None
}
