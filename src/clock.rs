//! Время суток для журнала: локальные часы без сторонних зависимостей.

#![cfg_attr(not(feature = "gui"), allow(dead_code))]

/// Локальное время в формате `ЧЧ:ММ:СС`. Если системное время недоступно,
/// возвращается время UTC — формат при этом не меняется.
pub fn hh_mm_ss() -> String {
    #[cfg(windows)]
    if let Some((h, m, s)) = win_local() {
        return format!("{h:02}:{m:02}:{s:02}");
    }
    #[cfg(unix)]
    if let Some((h, m, s)) = unix_local() {
        return format!("{h:02}:{m:02}:{s:02}");
    }
    let (h, m, s) = utc();
    format!("{h:02}:{m:02}:{s:02}")
}

#[cfg(windows)]
fn win_local() -> Option<(u16, u16, u16)> {
    #[repr(C)]
    struct SystemTime {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        milliseconds: u16,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetLocalTime(time: *mut SystemTime);
    }
    let mut time = SystemTime {
        year: 0,
        month: 0,
        day_of_week: 0,
        day: 0,
        hour: 0,
        minute: 0,
        second: 0,
        milliseconds: 0,
    };
    // SAFETY: структура целиком заполняется системным вызовом; других ссылок нет.
    unsafe { GetLocalTime(&mut time) };
    if time.year == 0 {
        None
    } else {
        Some((time.hour, time.minute, time.second))
    }
}

#[cfg(unix)]
fn unix_local() -> Option<(u16, u16, u16)> {
    #[repr(C)]
    struct Tm {
        sec: i32,
        min: i32,
        hour: i32,
        mday: i32,
        mon: i32,
        year: i32,
        wday: i32,
        yday: i32,
        isdst: i32,
        gmtoff: std::ffi::c_long,
        zone: *const std::ffi::c_char,
    }
    unsafe extern "C" {
        fn localtime_r(time: *const i64, out: *mut Tm) -> *mut Tm;
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    let mut tm = Tm {
        sec: 0,
        min: 0,
        hour: 0,
        mday: 1,
        mon: 0,
        year: 70,
        wday: 0,
        yday: 0,
        isdst: 0,
        gmtoff: 0,
        zone: std::ptr::null(),
    };
    // SAFETY: localtime_r заполняет переданную структуру и не удерживает указателей.
    let ok = unsafe { localtime_r(&secs, &mut tm) };
    if ok.is_null() {
        None
    } else {
        Some((tm.hour as u16, tm.min as u16, tm.sec as u16))
    }
}

fn utc() -> (u64, u64, u64) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let day = secs % 86_400;
    (day / 3600, (day % 3600) / 60, day % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_is_two_digits_with_colons() {
        let text = hh_mm_ss();
        assert_eq!(text.len(), 8, "ожидалось ЧЧ:ММ:СС, получено {text}");
        let parts: Vec<&str> = text.split(':').collect();
        assert_eq!(parts.len(), 3);
        for part in parts {
            assert_eq!(part.len(), 2);
            assert!(part.chars().all(|c| c.is_ascii_digit()));
        }
    }

    #[test]
    fn utc_stays_within_day() {
        let (h, m, s) = utc();
        assert!(h < 24 && m < 60 && s < 60);
    }
}
