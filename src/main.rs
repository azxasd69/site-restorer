//! Site Restorer (Rust): разбор адреса, ядро (CDX + загрузка + сборка), нативный GUI и CLI.

#[cfg(feature = "gui")]
mod app;
mod core;
mod i18n;
mod paths;
mod sanitize;

/// Версия берётся из Cargo.toml.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

use core::{Engine, Progress};
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
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
                println!(
                    "[{:5.1}%] {}  {} | {:.0} КБ/с потоков={} ok={} нет={} ошибок={}",
                    p.overall,
                    p.stage,
                    p.text,
                    p.bps / 1024.0,
                    p.threads,
                    p.ok,
                    p.missing,
                    p.errors
                );
                let _ = p.eta;
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
