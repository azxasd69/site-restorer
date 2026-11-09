//! Нативное окно (egui 0.37): форма, живой прогресс, журнал, запуск ядра.

use crate::core::{Engine, Progress};
use crate::paths;
use crate::sanitize::{sanitize_site, Note};
use eframe::egui;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;

#[derive(Clone, Copy)]
enum Level {
    Ok,
    Warn,
    Err,
}

enum Msg {
    Progress(Progress),
    Done(Result<String, String>),
}

pub struct App {
    site: String,
    out: String,
    note: String,
    log: Vec<(String, Level)>,
    show_help: bool,
    running: bool,
    overall: f32,
    stage: String,
    status: String,
    done: usize,
    total: usize,
    ok: usize,
    missing: usize,
    errors: usize,
    bytes: u64,
    bps: f64,
    threads: usize,
    types: [usize; 8],
    rx: Option<Receiver<Msg>>,
    cancel: Option<Arc<AtomicBool>>,
    pending_start: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::light());
        Self {
            site: std::env::var("SR_AUTOSTART").unwrap_or_default(),
            out: std::env::var("SR_OUT").unwrap_or_else(|_| default_out()),
            note: "Вставьте адрес — можно ссылку из письма, поиска или архива.".to_string(),
            log: vec![(
                "Готов к запуску. Адрес разбирается автоматически.".to_string(),
                Level::Warn,
            )],
            show_help: false,
            running: false,
            overall: 0.0,
            stage: "Готов к запуску".to_string(),
            status: "Ожидание запуска".to_string(),
            done: 0,
            total: 0,
            ok: 0,
            missing: 0,
            errors: 0,
            bytes: 0,
            bps: 0.0,
            threads: 0,
            types: [0; 8],
            rx: None,
            cancel: None,
            pending_start: std::env::var("SR_AUTOSTART").map(|v| !v.is_empty()).unwrap_or(false),
        }
    }

    fn push_log(&mut self, text: impl Into<String>, level: Level) {
        self.log.push((text.into(), level));
        if self.log.len() > 500 {
            self.log.remove(0);
        }
    }

    fn analyse(&mut self) {
        match sanitize_site(&self.site) {
            Some((host, note)) => {
                let why = match note {
                    Note::Plain => "обычный адрес",
                    Note::Layout => "была русская раскладка",
                    Note::Idn => "кириллический домен -> punycode",
                };
                let entry = paths::url_to_local_path(&format!("https://{host}/"));
                self.note = format!("→ {host}  ({why})   файл в копии: {entry}");
            }
            None => {
                self.note = "⚠ Не разобрал адрес — проверьте написание домена.".to_string();
            }
        }
    }

    fn start(&mut self) {
        if self.running {
            return;
        }
        let Some((host, _)) = sanitize_site(&self.site) else {
            self.push_log("не разобрал адрес", Level::Err);
            return;
        };
        let out = PathBuf::from(self.out.trim());
        if self.out.trim().is_empty() {
            self.push_log("укажите папку для копии", Level::Err);
            return;
        }
        let Some(mut engine) = Engine::new(&host, &out, 16, 0) else {
            self.push_log("движок не создан", Level::Err);
            return;
        };

        let (tx, rx) = mpsc::channel();
        let tx_progress = tx.clone();
        engine.set_progress_handler(move |p: &Progress| {
            let _ = tx_progress.send(Msg::Progress(p.clone()));
        });
        self.cancel = Some(engine.cancel_token());
        self.rx = Some(rx);
        self.running = true;
        self.overall = 0.0;
        self.stage = "Инвентаризация архива".to_string();
        self.status = "Подготовка…".to_string();
        self.push_log(format!("старт: {host} → {}", out.display()), Level::Ok);

        std::thread::spawn(move || {
            let result = engine
                .run()
                .map(|o| {
                    format!(
                        "готово: скачано {} из {}, страниц {}, битых ссылок {}. Отчёт: {}",
                        o.ok,
                        o.total,
                        o.stats.pages,
                        o.stats.broken,
                        o.report.display()
                    )
                })
                .map_err(|e| e);
            let _ = tx.send(Msg::Done(result));
        });
    }

    fn stop(&mut self) {
        if let Some(cancel) = &self.cancel {
            cancel.store(true, Ordering::Relaxed);
            self.push_log("остановка…", Level::Warn);
        }
    }

    fn poll(&mut self) {
        let Some(rx) = self.rx.take() else { return };
        let mut keep = true;
        loop {
            match rx.try_recv() {
                Ok(Msg::Progress(p)) => {
                    if p.stage == "log" {
                        self.push_log(p.text.clone(), Level::Warn);
                        continue;
                    }
                    self.overall = p.overall;
                    self.stage = match p.stage {
                        "inventory" => "Инвентаризация архива".to_string(),
                        "download" => "Скачивание файлов".to_string(),
                        "build" => "Сборка оффлайн-копии".to_string(),
                        "verify" => "Проверка ссылок".to_string(),
                        "report" => "Отчёт".to_string(),
                        other => other.to_string(),
                    };
                    if !p.text.is_empty() {
                        self.status = p.text.clone();
                    }
                    self.done = p.done;
                    self.total = p.total;
                    self.ok = p.ok;
                    self.missing = p.missing;
                    self.errors = p.errors;
                    self.bytes = p.bytes;
                    self.bps = p.bps;
                    self.threads = p.threads;
                    for (idx, entry) in p.types.iter().enumerate() {
                        self.types[idx * 2] = entry.1;
                        self.types[idx * 2 + 1] = entry.2;
                    }
                }
                Ok(Msg::Done(result)) => {
                    keep = false;
                    self.running = false;
                    self.cancel = None;
                    self.stage = "Готово".to_string();
                    match result {
                        Ok(text) => {
                            self.overall = 100.0;
                            self.push_log(text, Level::Ok);
                            self.status = "Копия собрана".to_string();
                        }
                        Err(err) if err == "cancelled" => {
                            self.push_log("остановлено; повторный запуск продолжит", Level::Warn);
                            self.status = "Остановлено".to_string();
                        }
                        Err(err) => {
                            self.push_log(format!("ошибка: {err}"), Level::Err);
                            self.status = "Ошибка".to_string();
                        }
                    }
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    keep = false;
                    self.running = false;
                    break;
                }
            }
        }
        if keep {
            self.rx = Some(rx);
        }
    }
}

fn default_out() -> String {
    match std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        Ok(home) => format!("{home}/SiteRestore/site"),
        Err(_) => "SiteRestore/site".to_string(),
    }
}

/// Полоса прогресса: рисуем сами, чтобы она растягивалась по ширине.
fn bar(ui: &mut egui::Ui, frac: f32, height: f32, width: f32) {
    let width = width.min(ui.available_width()).max(24.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let radius = egui::CornerRadius::same((height / 2.0) as u8);
    let painter = ui.painter();
    painter.rect_filled(rect, radius, egui::Color32::from_rgb(226, 232, 240));
    if frac > 0.0 {
        let filled =
            egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * frac.clamp(0.0, 1.0), rect.height()));
        painter.rect_filled(filled, radius, egui::Color32::from_rgb(37, 99, 235));
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll();
        if self.pending_start && !self.running {
            self.pending_start = false;
            self.start();
        }
        if self.running {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
        }

        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(10.0);
            ui.heading("Восстановление сайта из веб-архива");
            ui.label(
                egui::RichText::new(
                    "Скачивает сохранённую копию сайта из Wayback Machine и собирает её для локального просмотра.",
                )
                .weak(),
            );
            ui.add_space(10.0);

            egui::Grid::new("form")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Адрес сайта");
                    ui.add_enabled(
                        !self.running,
                        egui::TextEdit::singleline(&mut self.site)
                            .hint_text("например, prokupol.ru")
                            .desired_width(f32::INFINITY),
                    );
                    ui.end_row();

                    ui.label("Папка для копии");
                    ui.horizontal(|ui| {
                        ui.add_enabled(
                            !self.running,
                            egui::TextEdit::singleline(&mut self.out)
                                .desired_width(ui.available_width() - 110.0),
                        );
                        if ui.add_enabled(!self.running, egui::Button::new("Разобрать")).clicked() {
                            self.analyse();
                        }
                    });
                    ui.end_row();
                });

            ui.add_space(6.0);
            ui.label(egui::RichText::new(&self.note).weak());
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                let start = ui.add_enabled(!self.running, egui::Button::new("Начать"));
                if start.clicked() {
                    self.start();
                }
                if ui.add_enabled(self.running, egui::Button::new("Остановить")).clicked() {
                    self.stop();
                }
                if ui.button("Открыть папку").clicked() {
                    open_folder(&self.out);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Справка").clicked() {
                        self.show_help = true;
                    }
                });
            });
            ui.add_space(10.0);
        });

        egui::Panel::bottom("log")
            .resizable(true)
            .show(ui, |ui| {
                ui.add_space(6.0);
                ui.label(egui::RichText::new("Журнал").strong());
                egui::Frame::new()
                    .fill(egui::Color32::from_rgb(17, 24, 39))
                    .inner_margin(egui::Margin::same(10))
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .stick_to_bottom(true)
                            .auto_shrink([false, false])
                            .max_height(180.0)
                            .show(ui, |ui| {
                                for (text, level) in &self.log {
                                    let color = match level {
                                        Level::Ok => egui::Color32::from_rgb(74, 222, 128),
                                        Level::Warn => egui::Color32::from_rgb(251, 191, 36),
                                        Level::Err => egui::Color32::from_rgb(248, 113, 113),
                                    };
                                    ui.label(
                                        egui::RichText::new(text).monospace().color(color),
                                    );
                                }
                            });
                    });
            });

        egui::CentralPanel::default().show(ui, |ui| {
            egui::Frame::new()
                .fill(egui::Color32::WHITE)
                .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(229, 231, 235)))
                .inner_margin(egui::Margin::same(14))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(&self.stage).strong());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(format!("{:.0} %", self.overall));
                        });
                    });
                    ui.add_space(6.0);
                    let full = ui.available_width();
                    bar(ui, self.overall / 100.0, 14.0, full);
                    ui.add_space(10.0);
                    let names = ["HTML-страницы", "CSS / JS / XML", "Изображения", "Прочие файлы"];
                    for (idx, name) in names.iter().enumerate() {
                        ui.horizontal(|ui| {
                            ui.add_sized([140.0, 18.0], egui::Label::new(*name));
                            let ok = self.types[idx * 2];
                            let total = self.types[idx * 2 + 1];
                            let frac = if total > 0 { ok as f32 / total as f32 } else { 0.0 };
                            let width = ui.available_width() - 70.0;
                            bar(ui, frac, 10.0, width);
                            ui.label(format!("{ok} / {total}"));
                        });
                    }
                    ui.add_space(10.0);
                    ui.separator();
                    ui.label(&self.status);
                    if self.total > 0 {
                        ui.label(
                            egui::RichText::new(format!(
                                "скачано {} из {} · {:.0} КБ/с · потоков {} · нет в архиве {} · ошибок {}",
                                self.done, self.total, self.bps / 1024.0, self.threads, self.missing, self.errors
                            ))
                            .weak(),
                        );
                    }
                    if self.bytes > 0 {
                        ui.label(
                            egui::RichText::new(format!(
                                "объём: {:.1} МБ",
                                self.bytes as f64 / 1_048_576.0
                            ))
                            .weak(),
                        );
                    }
                });
        });

        if self.show_help {
            let ctx = ui.ctx().clone();
            let mut open = self.show_help;
            egui::Window::new("Справка — Site Restorer")
                .open(&mut open)
                .default_size([620.0, 420.0])
                .show(&ctx, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        ui.label(HELP_TEXT);
                    });
                });
            self.show_help = open;
        }
    }
}

fn open_folder(path: &str) {
    if path.trim().is_empty() {
        return;
    }
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(not(target_os = "windows"))]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

const HELP_TEXT: &str = "ЧТО ДЕЛАЕТ ПРОГРАММА\n\
Скачивает из Wayback Machine (Internet Archive) все сохранённые страницы и файлы\n\
указанного сайта и собирает статическую копию для локального просмотра.\n\n\
ФОРМАТЫ АДРЕСА (можно вставлять как угодно)\n\
prokupol.ru · https://www.prokupol.ru/catalog/ · PROKUPOL.RU\n\
«prokupol.ru» · Сайт: prokupol.ru — ссылка из письма\n\
ссылка из архива: web.archive.org/web/2026.../https://prokupol.ru/\n\
ссылка из поиска: google.com/url?q=https://prokupol.ru/\n\
адрес, набранный в русской раскладке: зкщлгзщдюкг\n\
кириллический домен: прокупол.рф\n\n\
ЧТО ВОССТАНАВЛИВАЕТСЯ\n\
Только то, что архив когда-либо сохранил. Файлы, которые краулер не забирал,\n\
отсутствуют — их нет ни в одной версии. Динамика (корзина, вход, поиск)\n\
в статической копии не работает.\n\n\
РЕЗУЛЬТАТ\n\
<папка>/site        — готовая копия (открывать локальным веб-сервером)\n\
<папка>/mirror      — исходные файлы из архива\n\
<папка>/report.html — отчёт о покрытии\n\n\
КОМАНДНАЯ СТРОКА\n\
site-restorer --cli --site <адрес> --out <папка> [--limit N] [--workers N]\n";
