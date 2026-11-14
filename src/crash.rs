//! Аварийный след паники (Д-95).
//!
//! `panic = "abort"` означает, что паника убивает процесс без разбора: в журнале
//! окна и в консоли остаётся пусто, и разобрать «программа просто закрылась»
//! невозможно. Хук пишет время, поток, место, сообщение и backtrace в
//! `data/crash.log` рядом с копией, не подавляя штатный вывод.
//!
//! В оконной сборке после записи показывается сообщение с путём к файлу: у GUI нет
//! консоли, иначе пользователь видит только исчезнувшее окно.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Имя файла следа.
pub const LOG_NAME: &str = "crash.log";

/// Куда писать след: `<--out>/data/crash.log` (по умолчанию `SiteRestore/site/data`).
///
/// Разбор здесь свой, а не общий: хук ставится до разбора аргументов, и при
/// кривом аргументе `--out` процесс обязан всё равно оставить след.
pub fn log_path(args: &[String]) -> PathBuf {
    let mut out = PathBuf::from("SiteRestore/site");
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--out"
            && let Some(value) = args.get(i + 1)
            && !value.starts_with('-')
        {
            out = PathBuf::from(value);
            i += 1;
        }
        i += 1;
    }
    out.join("data").join(LOG_NAME)
}

/// Метка времени `ГГГГ-ММ-ДД чч:мм:ссZ` из `ГГГГММДДччммсс`.
fn readable(stamp: &str) -> String {
    if stamp.len() < 14 {
        return stamp.to_string();
    }
    format!(
        "{}-{}-{} {}:{}:{}Z",
        &stamp[0..4],
        &stamp[4..6],
        &stamp[6..8],
        &stamp[8..10],
        &stamp[10..12],
        &stamp[12..14]
    )
}

/// Текст паники: строка или `String`, иначе — общее описание.
fn payload_text(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        return (*text).to_string();
    }
    if let Some(text) = payload.downcast_ref::<String>() {
        return text.clone();
    }
    "паника без текста".to_string()
}

/// Дописать одну запись в след. Каталог создаётся при необходимости.
pub fn record(
    path: &Path,
    thread: &str,
    location: &str,
    message: &str,
    backtrace: &str,
) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(
        file,
        "{} thread={} at {}\r\npanic: {}",
        readable(&crate::core::stamp_now()),
        thread,
        location,
        message
    )?;
    writeln!(file, "--- backtrace ---\r\n{backtrace}\r\n--- end ---")?;
    file.flush()
}

/// Поставить хук: пишет след и показывает сообщение в оконной сборке.
pub fn install(path: PathBuf) {
    install_with(path, true);
}

/// То же с управлением сообщением: тесты ставят хук без окна.
pub fn install_with(path: PathBuf, popup: bool) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current()
            .name()
            .unwrap_or("без имени")
            .to_string();
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "место неизвестно".to_string());
        let message = payload_text(info.payload());
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();
        let written = record(&path, &thread, &location, &message, &backtrace);
        // Штатный вывод не подавляем: в консольной сборке сообщение о панике
        // должно остаться на месте (Д-95).
        previous(info);
        if popup {
            let text = match &written {
                Ok(()) => format!(
                    "Программа остановилась из-за внутренней ошибки.\n\nСлед записан: {}\n\nПришлите этот файл вместе с описанием того, что делали.",
                    path.display()
                ),
                Err(err) => format!(
                    "Программа остановилась из-за внутренней ошибки.\n\nСлед записать не удалось ({err}).\nПуть: {}",
                    path.display()
                ),
            };
            crate::message_box(&text);
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sr-crash-{tag}-{}-{}",
            std::process::id(),
            crate::core::stamp_now()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Д-95: паника в рабочем потоке оставляет файл с потоком и сообщением.
    #[test]
    fn panic_in_worker_is_recorded() {
        let dir = temp_dir("worker");
        let path = dir.join("data").join(LOG_NAME);
        install_with(path.clone(), false);
        let handle = std::thread::Builder::new()
            .name("engine-worker".to_string())
            .spawn(|| panic!("намеренная паника для теста Д-95"))
            .expect("поток должен запуститься");
        // Паника потока не пробрасывается в тест: join вернёт Err, и это норма.
        let _ = handle.join();

        let text = std::fs::read_to_string(&path).expect("файл следа должен появиться");
        // Печать под --nocapture: так проверка «след есть» видна глазами, а не
        // только по коду возврата теста.
        println!("--- crash.log ---\n{text}--- конец ---");
        assert!(
            text.contains("thread=engine-worker"),
            "нет имени потока:\n{text}"
        );
        assert!(
            text.contains("намеренная паника для теста Д-95"),
            "нет сообщения паники:\n{text}"
        );
        assert!(text.contains("panic:"), "нет строки panic:\n{text}");
        assert!(text.contains("--- backtrace ---"), "нет backtrace:\n{text}");
        assert!(
            text.contains("crash.rs") || text.contains("src/"),
            "нет места паники:\n{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Д-95: файл не теряет прежние записи — след дописывается.
    #[test]
    fn record_appends() {
        let dir = temp_dir("append");
        let path = dir.join(LOG_NAME);
        record(&path, "первый", "a.rs:1:1", "раз", "bt1").expect("запись");
        record(&path, "второй", "b.rs:2:2", "два", "bt2").expect("запись");
        let text = std::fs::read_to_string(&path).expect("файл");
        assert!(
            text.contains("thread=первый") && text.contains("thread=второй"),
            "{text}"
        );
        assert!(
            text.contains("panic: раз") && text.contains("panic: два"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Путь берётся из `--out`, иначе — каталог по умолчанию.
    #[test]
    fn log_path_follows_out() {
        let with_out: Vec<String> = ["--out", r"D:\tmp\copy"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(log_path(&with_out).ends_with(Path::new("data").join(LOG_NAME)));
        assert_eq!(
            log_path(&with_out),
            PathBuf::from(r"D:\tmp\copy").join("data").join(LOG_NAME)
        );
        let empty: Vec<String> = Vec::new();
        assert_eq!(
            log_path(&empty),
            PathBuf::from("SiteRestore/site")
                .join("data")
                .join(LOG_NAME)
        );
    }
}
