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
#[cfg(feature = "gui")]
mod clip;
mod clock;
mod core;
mod encoding;
#[cfg(feature = "gui")]
mod i18n;
#[cfg(feature = "gui")]
mod links;
mod paths;
#[cfg(feature = "gui")]
mod pick;
mod puny;
mod sanitize;
mod settings;
mod tr;

/// Версия берётся из Cargo.toml.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

use core::{Engine, Progress, SnapshotMode};
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

/// Консоль Windows по умолчанию использует кодовую страницу OEM (866 в русской
/// системе), а мы пишем UTF-8. Переключаем консоль на UTF-8, иначе в cmd.exe и
/// PowerShell 5.1 пользователь видит кракозябры.
#[cfg(windows)]
fn ensure_console_utf8() {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetConsoleOutputCP(code_page: u32) -> i32;
        fn SetConsoleCP(code_page: u32) -> i32;
    }
    const CP_UTF8: u32 = 65001;
    // SAFETY: меняем кодовую страницу текущей консоли; при её отсутствии вызов
    // просто возвращает 0.
    unsafe {
        SetConsoleOutputCP(CP_UTF8);
        SetConsoleCP(CP_UTF8);
    }
}

/// Сообщение об ошибке отдельным окном: у GUI-сборки нет консоли, иначе
/// пользователь видит только «ничего не произошло».
#[cfg(all(windows, feature = "gui"))]
fn message_box(text: &str) {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn MessageBoxW(
            hwnd: *mut std::ffi::c_void,
            text: *const u16,
            caption: *const u16,
            flags: u32,
        ) -> i32;
    }
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let body = wide(text);
    let caption = wide("Site Restorer");
    const MB_ICONERROR: u32 = 0x10;
    // SAFETY: указатели на нуль-терминированные строки живут до конца вызова.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            body.as_ptr(),
            caption.as_ptr(),
            MB_ICONERROR,
        );
    }
}

#[cfg(not(all(windows, feature = "gui")))]
#[allow(dead_code)] // нужна только GUI-сборке
fn message_box(text: &str) {
    eprintln!("{text}");
}

#[cfg(not(windows))]
fn ensure_console_utf8() {}

#[cfg(not(windows))]
fn attach_parent_console() {}

/// Инициализация COM в режиме apartment-threaded (идемпотентна, ошибки не критичны).
#[cfg(all(windows, feature = "gui"))]
unsafe fn init_com() {
    #[link(name = "ole32")]
    unsafe extern "system" {
        fn OleInitialize(reserved: *mut std::ffi::c_void) -> i32;
    }
    // SAFETY: вызов до старта оконного цикла, на главном потоке.
    let _ = unsafe { OleInitialize(std::ptr::null_mut()) };
}

/// Есть ли графический сеанс: нужен для понятного сообщения до открытия окна (Д-13).
/// На Linux без `DISPLAY` и `WAYLAND_DISPLAY` окно не откроется, и об этом лучше сказать сразу.
#[cfg(all(feature = "gui", unix))]
fn graphics_available() -> bool {
    let display = std::env::var("DISPLAY").unwrap_or_default();
    let wayland = std::env::var("WAYLAND_DISPLAY").unwrap_or_default();
    !display.trim().is_empty() || !wayland.trim().is_empty()
}

#[cfg(all(feature = "gui", not(unix)))]
fn graphics_available() -> bool {
    true
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let console_mode = args
        .iter()
        .any(|a| a == "--cli" || a == "--version" || a == "-V");
    if console_mode || !cfg!(feature = "gui") {
        attach_parent_console();
        ensure_console_utf8();
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
        // Д-13: до открытия окна проверяем, есть ли графический сеанс. Без него
        // окно не откроется, а сообщение об этом должно называть пакеты и консоль.
        if !graphics_available() {
            let cli = cli_build_name();
            let text = if std::env::var("SR_LANG").as_deref() == Ok("en") {
                format!(
                    "No graphics session found (DISPLAY and WAYLAND_DISPLAY are empty).\n\nInstall the graphics libraries: libgl1 libegl1 libxkbcommon-x11-0 libwayland-client0 (Debian/Ubuntu) or mesa-libGL libxkbcommon wayland (Fedora).\nOr use the command-line build: {cli} --cli --site <address>"
                )
            } else {
                format!(
                    "Графический сеанс не найден (переменные DISPLAY и WAYLAND_DISPLAY пусты).\n\nУстановите библиотеки графики: libgl1 libegl1 libxkbcommon-x11-0 libwayland-client0 (Debian/Ubuntu) или mesa-libGL libxkbcommon wayland (Fedora).\nЛибо используйте консольную сборку: {cli} --cli --site <адрес>"
                )
            };
            eprintln!("{text}");
            message_box(&text);
            std::process::exit(3);
        }
        // COM нужен до создания окна: без него чтение буфера обмена, которым
        // владеет другой процесс (браузер, блокнот), возвращает пустоту —
        // Ctrl+V «не работает», хотя копирование внутри окна работает.
        #[cfg(windows)]
        unsafe {
            init_com();
        }
        let lang = if std::env::var("SR_LANG").as_deref() == Ok("en") {
            i18n::Lang::En
        } else {
            i18n::Lang::Ru
        };
        let title = links::window_title(lang);
        let options = eframe::NativeOptions {
            renderer: eframe::Renderer::Glow,
            // На Windows вставку и копирование ведём сами (см. src/clip.rs):
            // встроенный путь иногда не видит буфер другого процесса.
            clipboard_shortcuts: !cfg!(windows),
            persist_window: false,
            viewport: eframe::egui::ViewportBuilder::default()
                .with_inner_size([900.0, 660.0])
                .with_min_inner_size([760.0, 560.0])
                .with_icon(window_icon())
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
            let cli = cli_build_name();
            let text = if std::env::var("SR_LANG").as_deref() == Ok("en") {
                format!(
                    "Cannot open the window.\n\n{err}\n\nThe graphics driver does not provide OpenGL 2.0+ (typical for virtual machines and remote sessions).\nUse the command-line build: {cli} --cli --site <address>"
                )
            } else {
                format!(
                    "Не удалось открыть окно.\n\n{err}\n\nВидеодрайвер не предоставляет OpenGL 2.0+ (обычное дело в виртуальных машинах и удалённых сеансах).\nПроверьте пакеты графики: libgl1 libegl1 libxkbcommon-x11-0 libwayland-client0.\nВоспользуйтесь консольной сборкой: {cli} --cli --site <адрес>"
                )
            };
            message_box(&text);
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
    /// Сколько потоков отдавать сборке копии; 0 — по числу ядер.
    threads: usize,
    rate: f64,
    lang: String,
    /// Режим выбора снимка: newest (по умолчанию), oldest или дата (ФТ-126).
    snapshot: String,
    /// Дата снимка для режима «на дату» (ФТ-126).
    as_of: String,
    /// Сколько более старых снимков пробовать (ФТ-127).
    fallback_depth: Option<usize>,
    /// Срок свежести снимка в днях (ФТ-130).
    refresh_days: Option<u32>,
    /// Проверить свежесть всех файлов (ФТ-130).
    refresh: bool,
    /// Окно адресов для страниц (ФТ-128).
    since: String,
    /// Предел добора по ссылкам (ФТ-128).
    rescue_limit: Option<usize>,
    index_max_pages: Option<usize>,
    /// Бюджет времени на добор, секунд (ФТ-128).
    rescue_budget: Option<u64>,
    /// Через сколько дней повторять адреса без снимка (ФТ-135).
    retry_missing_days: Option<u32>,
    /// Порог «пустой страницы» (ФТ-127).
    stub_bytes: Option<usize>,
}

fn parse_args(args: &[String]) -> Args {
    let mut parsed = Args {
        site: String::new(),
        out: PathBuf::from("SiteRestore/site"),
        limit: 0,
        workers: 16,
        threads: 0,
        rate: 0.0,
        lang: "ru".to_string(),
        snapshot: "newest".to_string(),
        as_of: String::new(),
        fallback_depth: None,
        refresh_days: None,
        refresh: false,
        since: String::new(),
        rescue_limit: None,
        index_max_pages: None,
        rescue_budget: None,
        retry_missing_days: None,
        stub_bytes: None,
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
            "--threads" => {
                parsed.threads = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(0);
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
            "--snapshot" => {
                parsed.snapshot = args.get(i + 1).cloned().unwrap_or_default();
                i += 2;
            }
            "--as-of" => {
                parsed.as_of = args.get(i + 1).cloned().unwrap_or_default();
                i += 2;
            }
            "--fallback-depth" => {
                parsed.fallback_depth = args.get(i + 1).and_then(|v| v.parse().ok());
                i += 2;
            }
            "--refresh-days" => {
                parsed.refresh_days = args.get(i + 1).and_then(|v| v.parse().ok());
                i += 2;
            }
            "--refresh" => {
                parsed.refresh = true;
                i += 1;
            }
            "--since" => {
                parsed.since = args.get(i + 1).cloned().unwrap_or_default();
                i += 2;
            }
            "--index-max-pages" => {
                parsed.index_max_pages = args.get(i + 1).and_then(|v| v.parse().ok());
                i += 2;
            }
            "--rescue-limit" => {
                parsed.rescue_limit = args.get(i + 1).and_then(|v| v.parse().ok());
                i += 2;
            }
            "--rescue-budget" => {
                parsed.rescue_budget = args.get(i + 1).and_then(|v| v.parse().ok());
                i += 2;
            }
            "--retry-missing" => {
                parsed.retry_missing_days = args.get(i + 1).and_then(|v| v.parse().ok());
                i += 2;
            }
            "--stub-bytes" => {
                parsed.stub_bytes = args.get(i + 1).and_then(|v| v.parse().ok());
                i += 2;
            }
            _ => i += 1,
        }
    }
    parsed
}

fn cli(args: &[String]) -> i32 {
    let parsed = parse_args(args);
    // Язык действует и на окно, и на сообщения ядра/CLI.
    let english = parsed.lang == "en" || std::env::var("SR_LANG").as_deref() == Ok("en");
    tr::set_english(english);
    if english {
        // SAFETY: до запуска потоков, окружение больше не меняется.
        unsafe { std::env::set_var("SR_LANG", "en") };
    }
    if parsed.site.is_empty() {
        eprintln!(
            "{}",
            tr::t(
                "использование: site-restorer --cli --site <адрес> [--out <папка>] [--workers N] [--threads N] [--rate N] [--limit N] [--index-max-pages N]\n  снимки: [--snapshot newest|oldest|<дата>] [--as-of ГГГГ[-ММ[-ДД]]] [--fallback-depth N] [--refresh-days N] [--refresh] [--since ГГГГ[-ММ]] [--rescue-limit N] [--stub-bytes N]",
                "usage: site-restorer --cli --site <address> [--out <folder>] [--workers N] [--threads N] [--rate N] [--limit N] [--index-max-pages N]\n  snapshots: [--snapshot newest|oldest|<date>] [--as-of YYYY[-MM[-DD]]] [--fallback-depth N] [--refresh-days N] [--refresh] [--since YYYY[-MM]] [--rescue-limit N] [--stub-bytes N]",
            )
        );
        return 2;
    }
    let Some(mut engine) = Engine::new(&parsed.site, &parsed.out, parsed.workers, parsed.limit)
    else {
        eprintln!(
            "{}",
            tr::pick(
                format!("не разобрал адрес: {:?}", parsed.site),
                format!("cannot parse address: {:?}", parsed.site),
            )
        );
        return 2;
    };

    engine.set_rate(parsed.rate);
    engine.set_build_threads(parsed.threads);
    // Режим снимка: по умолчанию самый свежий; oldets и дата — для воспроизводимости (ФТ-126).
    let snapshot_text = if parsed.as_of.trim().is_empty() {
        parsed.snapshot.clone()
    } else {
        parsed.as_of.clone()
    };
    match SnapshotMode::parse(&snapshot_text) {
        Some(mode) => engine.set_snapshot(mode),
        None => {
            eprintln!(
                "{}",
                tr::pick(
                    format!("не разобрал режим снимка: {:?}", snapshot_text),
                    format!("cannot parse snapshot mode: {:?}", snapshot_text),
                )
            );
            return 2;
        }
    }
    if let Some(depth) = parsed.fallback_depth {
        engine.set_fallback_depth(depth);
    }
    if let Some(days) = parsed.refresh_days {
        engine.set_refresh_days(days);
    }
    if parsed.refresh {
        engine.set_refresh(true);
    }
    if !parsed.since.trim().is_empty() {
        let digits: String = parsed
            .since
            .chars()
            .filter(|c| c.is_ascii_digit())
            .collect();
        if digits.len() < 4 {
            eprintln!(
                "{}",
                tr::pick(
                    format!("не разобрал дату окна: {:?}", parsed.since),
                    format!("cannot parse the window date: {:?}", parsed.since),
                )
            );
            return 2;
        }
        let mut bound = digits;
        while bound.len() < 14 {
            bound.push('0');
        }
        engine.set_since(Some(bound));
    }
    if let Some(pages) = parsed.index_max_pages {
        engine.set_index_max_pages(pages);
    }
    if let Some(limit) = parsed.rescue_limit {
        engine.set_rescue_limit(limit);
    }
    if let Some(seconds) = parsed.rescue_budget {
        engine.set_rescue_budget(seconds);
    }
    if let Some(days) = parsed.retry_missing_days {
        engine.set_retry_missing_days(days);
    }
    if let Some(bytes) = parsed.stub_bytes {
        engine.set_stub_bytes(bytes);
    }
    engine.set_progress_handler(|p: &Progress| match p.stage {
        "log" => println!("{}", p.text),
        "download" => {
            if p.done.is_multiple_of(5) || p.done == p.total {
                let eta = if p.eta > 0 {
                    format!(" ~{} с", p.eta)
                } else {
                    String::new()
                };
                let pause = if p.pause > 0 {
                    tr::pick(format!(" пауза {} с", p.pause), format!(" pause {}s", p.pause))
                } else {
                    String::new()
                };
                println!(
                    "{}",
                    tr::pick(
                        format!(
                            "[{:5.1}%] {}  {} | {:.0} КБ/с файлов/с={:.1} потоков={} ok={} нет={} ошибок={} {:.1} МБ{}{}",
                            p.overall,
                            p.stage,
                            p.text,
                            p.bps / 1024.0,
                            p.files_per_sec,
                            p.threads,
                            p.ok,
                            p.missing,
                            p.errors,
                            p.bytes as f64 / 1_048_576.0,
                            eta,
                            pause
                        ),
                        format!(
                            "[{:5.1}%] {}  {} | {:.0} KB/s files/s={:.1} threads={} ok={} missing={} errors={} {:.1} MB{}{}",
                            p.overall,
                            p.stage,
                            p.text,
                            p.bps / 1024.0,
                            p.files_per_sec,
                            p.threads,
                            p.ok,
                            p.missing,
                            p.errors,
                            p.bytes as f64 / 1_048_576.0,
                            eta,
                            pause
                        ),
                    )
                );
                if p.done == p.total && p.total > 0 {
                    let names: Vec<String> = p
                        .types
                        .iter()
                        .map(|(name, ok, total)| format!("{name} {ok}/{total}"))
                        .collect();
                    println!("{}", tr::pick(format!("  готово по типам: {}", names.join(", ")), format!("  done by type: {}", names.join(", "))));
                }
            }
        }
        _ => {
            if !p.text.is_empty() {
                println!("[{:5.1}%] {}  {}", p.overall, p.stage, p.text);
                // При перенаправлении вывода в файл строка должна быть видна сразу:
                // без сброса буфера журнал читается рывками или только в конце (ОШ-4).
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
        }
    });

    match engine.run() {
        Ok(outcome) => {
            println!(
                "{}",
                tr::pick(
                    format!(
                        "ИТОГ: скачано {} из {} адресов, файлов в копии {}, страниц {}, ссылок {}, битых {}",
                        outcome.ok,
                        outcome.total,
                        outcome.site_files,
                        outcome.stats.pages,
                        outcome.stats.refs,
                        outcome.stats.broken
                    ),
                    format!(
                        "RESULT: downloaded {} of {} addresses, files in copy {}, pages {}, links {}, broken {}",
                        outcome.ok,
                        outcome.total,
                        outcome.site_files,
                        outcome.stats.pages,
                        outcome.stats.refs,
                        outcome.stats.broken
                    ),
                )
            );
            let (html, code, img, other) = (
                outcome.types[0],
                outcome.types[2],
                outcome.types[4],
                outcome.types[6],
            );
            let totals = (
                outcome.types[1],
                outcome.types[3],
                outcome.types[5],
                outcome.types[7],
            );
            println!(
                "{}",
                tr::pick(
                    format!(
                        "типы: HTML {html}/{}, CSS-JS {code}/{}, IMG {img}/{}, прочее {other}/{}",
                        totals.0, totals.1, totals.2, totals.3
                    ),
                    format!(
                        "types: HTML {html}/{}, CSS-JS {code}/{}, IMG {img}/{}, other {other}/{}",
                        totals.0, totals.1, totals.2, totals.3
                    ),
                )
            );
            println!(
                "{}",
                tr::pick(
                    format!(
                        "снимки: {} — {} (добор {}, без снимка {}, перезаписей {}, по ссылкам {})",
                        outcome.oldest,
                        outcome.newest,
                        outcome.fallbacks,
                        outcome
                            .total
                            .saturating_sub(outcome.ok)
                            .min(outcome.fallbacks),
                        outcome.collisions,
                        outcome.rescued
                    ),
                    format!(
                        "snapshots: {} — {} (fallbacks {}, no snapshot {}, collisions {}, rescued {})",
                        outcome.oldest,
                        outcome.newest,
                        outcome.fallbacks,
                        outcome
                            .total
                            .saturating_sub(outcome.ok)
                            .min(outcome.fallbacks),
                        outcome.collisions,
                        outcome.rescued
                    )
                )
            );
            println!(
                "{}",
                tr::pick(
                    format!("копия:  {}", outcome.site.display()),
                    format!("copy:   {}", outcome.site.display())
                )
            );
            println!(
                "{}",
                tr::pick(
                    format!("отчёт:  {}", outcome.report.display()),
                    format!("report: {}", outcome.report.display())
                )
            );
            // Машиночитаемый итог: одна строка, чтобы прогон разбирался скриптом.
            println!(
                "RESULT {{\"ok\":{},\"total\":{},\"files\":{},\"mirror\":{},\"unique\":{},\"pages\":{},\"foreign\":{},\"refs\":{},\"broken\":{},\"fallbacks\":{},\"collisions\":{},\"rescued\":{},\"oldest\":\"{}\",\"newest\":\"{}\",\"site\":\"{}\",\"report\":\"{}\"}}",
                outcome.ok,
                outcome.total,
                outcome.site_files,
                outcome.mirror_files,
                outcome.unique_paths,
                outcome.stats.pages,
                outcome.stats.foreign,
                outcome.stats.refs,
                outcome.stats.broken,
                outcome.fallbacks,
                outcome.collisions,
                outcome.rescued,
                outcome.oldest,
                outcome.newest,
                json_escape(&outcome.site.to_string_lossy()),
                json_escape(&outcome.report.to_string_lossy()),
            );
            0
        }
        Err(err) if err == "cancelled" => {
            println!(
                "{}",
                tr::t(
                    "остановлено; повторный запуск продолжит с места остановки",
                    "stopped; run again to continue from the same folder",
                )
            );
            130
        }
        Err(err) => {
            eprintln!(
                "{}",
                tr::pick(format!("ошибка: {err}"), format!("error: {err}"))
            );
            1
        }
    }
}

/// Иконка окна: знак продукта с сайта lastcopy.ru.
/// Байты готовит `scripts/make-icons.py` (64×64, RGBA); готовый массив — `packaging/icons/site-restorer-64.rgba`.
/// Декодировать PNG в рантайме не нужно, поэтому лишней зависимости нет.
#[cfg(feature = "gui")]
fn window_icon() -> eframe::egui::IconData {
    const RAW: &[u8] = include_bytes!("../packaging/icons/site-restorer-64.rgba");
    eframe::egui::IconData {
        rgba: RAW.to_vec(),
        width: 64,
        height: 64,
    }
}

/// Имя консольной сборки для текущей платформы и архитектуры.
/// Держим в одном месте: подсказка должна называть файл, который есть в релизе (ОШ-1).
/// Нужна только оконной сборке — в консольной её некому вызывать.
#[cfg(feature = "gui")]
fn cli_build_name() -> &'static str {
    if cfg!(all(target_os = "windows", target_arch = "x86")) {
        "SiteRestorer-windows-x86-cli.exe"
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "SiteRestorer-windows-x64-cli.exe"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "SiteRestorer-linux-x64-cli"
    } else {
        "SiteRestorer-windows-x64-cli.exe"
    }
}

/// Экранирование строки для JSON-итога.
fn json_escape(text: &str) -> String {
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
