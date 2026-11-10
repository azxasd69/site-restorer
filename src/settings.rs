//! Настройки пользователя: адрес, папка результата, язык и геометрия окна.
//!
//! Файл намеренно простой (плоский JSON): сериализатор ради пяти полей не нужен,
//! а терпимое чтение важнее строгости — повреждённый файл не должен мешать запуску.

#![cfg_attr(not(feature = "gui"), allow(dead_code))]

use std::path::{Path, PathBuf};

/// Положение и размер окна в точках экрана.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Window {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Настройки, сохраняемые между запусками.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub site: String,
    pub out: String,
    pub lang: String,
    pub window: Option<Window>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            site: String::new(),
            out: default_out(),
            lang: "ru".to_string(),
            window: None,
        }
    }
}

/// Папка результата по умолчанию: `<домашняя>/SiteRestore/site`.
pub fn default_out() -> String {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("SiteRestore")
        .join("site")
        .to_string_lossy()
        .into_owned()
}

/// Путь к файлу настроек в профиле пользователя.
pub fn config_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let base = std::env::var_os("APPDATA").map(PathBuf::from)?;
        Some(base.join("SiteRestorer").join("config.json"))
    }
    #[cfg(not(windows))]
    {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("site-restorer").join("config.json"))
    }
}

/// Прочитать настройки. Любая ошибка — значения по умолчанию.
pub fn load() -> Settings {
    match config_path() {
        Some(path) => load_from(&path),
        None => Settings::default(),
    }
}

/// Записать настройки. `false` — не удалось (нет прав, нет профиля).
pub fn save(settings: &Settings) -> bool {
    match config_path() {
        Some(path) => save_to(&path, settings),
        None => false,
    }
}

/// Удалить файл настроек: следующий запуск начнётся со значений по умолчанию.
pub fn reset() -> bool {
    match config_path() {
        Some(path) => std::fs::remove_file(path).is_ok(),
        None => false,
    }
}

pub fn load_from(path: &Path) -> Settings {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text),
        Err(_) => Settings::default(),
    }
}

pub fn save_to(path: &Path, settings: &Settings) -> bool {
    if let Some(dir) = path.parent()
        && std::fs::create_dir_all(dir).is_err()
    {
        return false;
    }
    // Пишем во временный файл и переименовываем: обрыв на середине записи
    // не должен оставить обрезанные настройки.
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, settings.render()).is_err() {
        return false;
    }
    if std::fs::rename(&tmp, path).is_ok() {
        true
    } else {
        let _ = std::fs::remove_file(&tmp);
        false
    }
}

impl Settings {
    pub fn render(&self) -> String {
        let mut fields = vec![
            format!("  \"site\": \"{}\"", escape(&self.site)),
            format!("  \"out\": \"{}\"", escape(&self.out)),
            format!("  \"lang\": \"{}\"", escape(&self.lang)),
        ];
        if let Some(w) = self.window {
            fields.push(format!("  \"window_x\": {}", w.x as i32));
            fields.push(format!("  \"window_y\": {}", w.y as i32));
            fields.push(format!("  \"window_w\": {}", w.w as i32));
            fields.push(format!("  \"window_h\": {}", w.h as i32));
        }
        format!("{{\n{}\n}}\n", fields.join(",\n"))
    }
}

/// Разобрать файл настроек: неизвестные поля игнорируются, кривые — отбрасываются.
pub fn parse(text: &str) -> Settings {
    let mut settings = Settings::default();
    let text = text.trim_start_matches('\u{feff}');
    if let Some(value) = string_value(text, "site") {
        settings.site = value;
    }
    if let Some(value) = string_value(text, "out")
        && !value.trim().is_empty()
    {
        settings.out = value;
    }
    if let Some(value) = string_value(text, "lang")
        && (value == "ru" || value == "en")
    {
        settings.lang = value;
    }
    let number = |key: &str| {
        raw_value(text, key)
            .filter(|(_, quoted)| !*quoted)
            .and_then(|(value, _)| value.parse::<f32>().ok())
    };
    if let (Some(x), Some(y), Some(w), Some(h)) = (
        number("window_x"),
        number("window_y"),
        number("window_w"),
        number("window_h"),
    ) && (480.0..=10_000.0).contains(&w)
        && (360.0..=10_000.0).contains(&h)
    {
        settings.window = Some(Window { x, y, w, h });
    }
    settings
}

fn string_value(text: &str, key: &str) -> Option<String> {
    raw_value(text, key)
        .filter(|(_, quoted)| *quoted)
        .map(|(value, _)| unescape(value))
}

/// Значение поля и признак «было в кавычках»: тип важен, иначе `42` не отличить от `"42"`.
fn raw_value<'a>(text: &'a str, key: &str) -> Option<(&'a str, bool)> {
    let needle = format!("\"{key}\"");
    let start = text.find(&needle)? + needle.len();
    let rest = &text[start..];
    let colon = rest.find(':')?;
    let rest = rest[colon + 1..].trim_start();
    if let Some(body) = rest.strip_prefix('"') {
        // Значение в кавычках: ищем закрывающую кавычку, учитывая экранирование.
        let mut escaped = false;
        for (idx, ch) in body.char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                '"' => return Some((&body[..idx], true)),
                _ => {}
            }
        }
        None
    } else {
        let end = rest.find([',', '}', '\n']).unwrap_or(rest.len());
        Some((rest[..end].trim(), false))
    }
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_keeps_values() {
        let settings = Settings {
            site: "rdbzh.ru".to_string(),
            out: "D:\\Копии\\site \"quote\"".to_string(),
            lang: "en".to_string(),
            window: Some(Window {
                x: 40.0,
                y: 60.0,
                w: 1000.0,
                h: 700.0,
            }),
        };
        let text = settings.render();
        assert_eq!(parse(&text), settings);
    }

    #[test]
    fn missing_or_broken_file_gives_defaults() {
        let defaults = Settings::default();
        assert_eq!(parse(""), defaults);
        assert_eq!(parse("{ \"site\": "), defaults);
        assert_eq!(parse("не json вовсе"), defaults);
        assert_eq!(parse("\u{feff}{}"), defaults);
    }

    #[test]
    fn invalid_values_are_dropped() {
        let text = r#"{
            "site": 42,
            "out": "",
            "lang": "de",
            "window_x": 10,
            "window_y": 20,
            "window_w": 100,
            "window_h": 80
        }"#;
        let settings = parse(text);
        assert_eq!(settings.site, "");
        assert_eq!(settings.out, Settings::default().out);
        assert_eq!(settings.lang, "ru");
        assert!(settings.window.is_none(), "слишком маленькое окно");
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let settings = parse(r#"{"site":"a.ru","future":"x","out":"/tmp/s"}"#);
        assert_eq!(settings.site, "a.ru");
        assert_eq!(settings.out, "/tmp/s");
    }

    #[test]
    fn save_and_load_through_file() {
        let dir = std::env::temp_dir().join(format!("sr-settings-{}", std::process::id()));
        let path = dir.join("config.json");
        let settings = Settings {
            site: "prokupol.ru".to_string(),
            out: dir.join("site").to_string_lossy().into_owned(),
            lang: "en".to_string(),
            window: None,
        };
        assert!(save_to(&path, &settings));
        assert_eq!(load_from(&path), settings);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
