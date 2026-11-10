//! Site Restorer (Rust): разбор адреса, ядро (CDX + загрузка + сборка), нативный GUI и CLI.
//!
//! GUI-сборка — приложение подсистемы windows: без консольного окна при запуске
//! двойным щелчком. Сборка только-CLI (`--no-default-features`) остаётся
//! консольной, поэтому вывод `--cli` и `--version` работает как обычно.
#![cfg_attr(
    all(windows, not(debug_assertions), feature = "gui"),
    windows_subsystem = "windows"
)]

#[cfg(feature = "gui")]
mod app;
mod core;
mod encoding;
#[cfg(feature = "gui")]
mod i18n;
mod paths;
mod sanitize;

/// Версия берётся из Cargo.toml.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

use core::{Engine, Progress};
use std::path::PathBuf;

/// В GUI-сборке подсистема windows, поэтому консоли у процесса нет. Если
/// пользователь всё же запустил `--cli`/`--version` из терминала, подключаемся
/// к консоли родителя: иначе вывод молча пропадёт.
#[cfg(windows)]
fn attach_parent_console() {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn AttachConsole(dw_process_id: u32) -> i32;
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            sec: *mut std::ffi::c_void,
            disposition: u32,
            flags: u32,
            template: *mut std::ffi::c_void,
        ) -> *mut std::ffi::c_void;
        fn SetStdHandle(kind: u32, handle: *mut std::ffi::c_void) -> i32;
        fn GetStdHandle(kind: u32) -> *mut std::ffi::c_void;
        fn GetFileType(handle: *mut std::ffi::c_void) -> u32;
    }
    const ATTACH_PARENT_PROCESS: u32 = 0xFFFF_FFFF;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const OPEN_EXISTING: u32 = 3;
    const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5;
    const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4;

    // SAFETY: вызовы WinAPI без предварительных условий; результат проверяется.
    unsafe {
        // Вывод уже перенаправлен в файл или пайп — не трогаем, иначе сломаем
        // скрипты и CI, которые читают stdout.
        let current = GetStdHandle(STD_OUTPUT_HANDLE);
        if !current.is_null() && current as isize != -1 {
            match GetFileType(current) {
                1 | 3 => return, // FILE_TYPE_DISK | FILE_TYPE_PIPE
                _ => {}
            }
        }
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return; // нет родительской консоли — молча работаем дальше
        }
        let name: Vec<u16> = "CONOUT$\0".encode_utf16().collect();
        let handle = CreateFileW(
            name.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_WRITE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        if handle as isize == -1 {
            return;
        }
        SetStdHandle(STD_OUTPUT_HANDLE, handle);
        SetStdHandle(STD_ERROR_HANDLE, handle);
    }
}

#[cfg(not(windows))]
fn attach_parent_console() {}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let console_mode = args.iter().any(|a| a == "--cli" || a == "--version" || a == "-V");
    if console_mode || !cfg!(feature = "gui") {
        attach_parent_console();
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("site-restorer {APP_VERSION}");
        return;
    }
    if args.iter().any(|a| a == "--cli") {
        std::process::exit(cli(&args));
    }

    #[cfg(feature = "gui")]
    {
        let title = if std::env::var("SR_LANG").as_deref() == Ok("en") {
            i18n::EN.title
        } else {
            i18n::RU.title
        };
        let options = eframe::NativeOptions {
            renderer: eframe::Renderer::Glow,
            viewport: eframe::egui::ViewportBuilder::default()
                .with_inner_size([880.0, 740.0])
                .with_min_inner_size([840.0, 700.0])
                .with_title(title),
            ..Default::default()
        };
        let run = eframe::run_native(
            "site-restorer",
            options,
            Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
        );
        if let Err(err) = run {
            eprintln!("не удалось открыть окно: {err}");
            std::process::exit(3);
        }
    }

    #[cfg(not(feature = "gui"))]
    {
        eprintln!("эта сборка собрана без GUI: используйте --cli --site <адрес>");
        std::process::exit(2);
    }
}

struct Args {
    site: String,
    out: PathBuf,
    limit: usize,
    workers: usize,
    rate: f64,
    lang: String,
}

fn parse_args(args: &[String]) -> Args {
    let mut parsed = Args {
        site: String::new(),
        out: PathBuf::from("SiteRestore/site"),
        limit: 0,
        workers: 16,
        rate: 0.0,
        lang: "ru".to_string(),
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--site" => {
                parsed.site = args.get(i + 1).cloned().unwrap_or_default();
                i += 2;
            }
            "--out" => {
                parsed.out = PathBuf::from(args.get(i + 1).cloned().unwrap_or_default());
                i += 2;
            }
            "--limit" => {
                parsed.limit = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(0);
                i += 2;
            }
            "--workers" => {
                parsed.workers = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(16);
                i += 2;
            }
            "--lang" => {
                parsed.lang = args.get(i + 1).cloned().unwrap_or_else(|| "ru".to_string());
                i += 2;
            }
            "--rate" => {
                parsed.rate = args
                    .get(i + 1)
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(0.0);
                i += 2;
            }
            _ => i += 1,
        }
    }
    parsed
}

fn cli(args: &[String]) -> i32 {
    let parsed = parse_args(args);
    // Язык влияет на GUI (окно читает SR_LANG); сообщения CLI остаются русскими.
    if parsed.lang == "en" {
        // SAFETY: до запуска потоков, окружение больше не меняется.
        unsafe { std::env::set_var("SR_LANG", "en") };
    }
    if parsed.site.is_empty() {
        eprintln!("использование: site-restorer --cli --site <адрес> [--out <папка>] [--workers N] [--rate N] [--limit N]");
        return 2;
    }
    let Some(mut engine) = Engine::new(&parsed.site, &parsed.out, parsed.workers, parsed.limit)
    else {
        eprintln!("не разобрал адрес: {:?}", parsed.site);
        return 2;
    };

    engine.set_rate(parsed.rate);
    engine.set_progress_handler(|p: &Progress| match p.stage {
        "log" => println!("{}", p.text),
        "download" => {
            if p.done % 5 == 0 || p.done == p.total {
                let eta = if p.eta > 0 {
                    format!(" ~{} с", p.eta)
                } else {
                    String::new()
                };
                println!(
                    "[{:5.1}%] {}  {} | {:.0} КБ/с потоков={} ok={} нет={} ошибок={} {:.1} МБ{}",
                    p.overall,
                    p.stage,
                    p.text,
                    p.bps / 1024.0,
                    p.threads,
                    p.ok,
                    p.missing,
                    p.errors,
                    p.bytes as f64 / 1_048_576.0,
                    eta
                );
                if p.done == p.total && p.total > 0 {
                    let names: Vec<String> = p
                        .types
                        .iter()
                        .map(|(name, ok, total)| format!("{name} {ok}/{total}"))
                        .collect();
                    println!("  готово по типам: {}", names.join(", "));
                }
            }
        }
        _ => {
            if !p.text.is_empty() {
                println!("[{:5.1}%] {}  {}", p.overall, p.stage, p.text);
            }
        }
    });

    match engine.run() {
        Ok(outcome) => {
            println!(
                "ИТОГ: скачано {} из {}, страниц {}, ссылок {}, битых {}",
                outcome.ok, outcome.total, outcome.stats.pages, outcome.stats.refs, outcome.stats.broken
            );
            let (html, code, img, other) = (
                outcome.types[0],
                outcome.types[2],
                outcome.types[4],
                outcome.types[6],
            );
            println!(
                "типы: HTML {html}/{}, CSS-JS {code}/{}, IMG {img}/{}, прочее {other}/{}",
                outcome.types[1], outcome.types[3], outcome.types[5], outcome.types[7]
            );
            println!("копия:  {}", outcome.site.display());
            println!("отчёт:  {}", outcome.report.display());
            0
        }
        Err(err) if err == "cancelled" => {
            println!("остановлено; повторный запуск продолжит с места остановки");
            130
        }
        Err(err) => {
            eprintln!("ошибка: {err}");
            1
        }
    }
}
