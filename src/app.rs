//! Нативное окно (egui 0.37): форма, живой прогресс, журнал, RU/EN.

use crate::core::{Engine, Progress};
use crate::i18n::Lang;
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

#[derive(Clone, Copy)]
enum Stage {
    Idle,
    Inventory,
    Download,
    Build,
    Verify,
    Report,
    Done,
    Stopped,
    Error,
}

pub struct App {
    site: String,
    out: String,
    note: String,
    log: Vec<(String, Level)>,
    lang: Lang,
    show_help: bool,
    running: bool,
    overall: f32,
    stage: Stage,
    status: String,
    done: usize,
    total: usize,
    ok: usize,
    missing: usize,
    errors: usize,
    bytes: u64,
    bps: f64,
    eta: u64,
    threads: usize,
    types: [usize; 8],
    rx: Option<Receiver<Msg>>,
    cancel: Option<Arc<AtomicBool>>,
    pending_start: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::light());
        let lang = match std::env::var("SR_LANG").as_deref() {
            Ok("en") => Lang::En,
            _ => Lang::Ru,
        };
        Self {
            site: std::env::var("SR_AUTOSTART").unwrap_or_default(),
            out: std::env::var("SR_OUT")
                .unwrap_or_else(|_| default_out()),
            note: lang.t().hint.to_string(),
            log: vec![(format!("Site Restorer {}", crate::APP_VERSION), Level::Warn)],
            lang,
            show_help: false,
            running: false,
            overall: 0.0,
            stage: Stage::Idle,
            status: lang.t().waiting.to_string(),
            done: 0,
            total: 0,
            ok: 0,
            missing: 0,
            errors: 0,
            bytes: 0,
            bps: 0.0,
            eta: 0,
            threads: 0,
            types: [0; 8],
            rx: None,
            cancel: None,
            pending_start: std::env::var("SR_AUTOSTART")
                .map(|v| !v.is_empty())
                .unwrap_or(false),
        }
    }

    fn stage_label(&self) -> &'static str {
        let t = self.lang.t();
        match self.stage {
            Stage::Idle => t.stage_idle,
            Stage::Inventory => t.stage_inventory,
            Stage::Download => t.stage_download,
            Stage::Build => t.stage_build,
            Stage::Verify => t.stage_verify,
            Stage::Report => t.stage_report,
            Stage::Done => t.stage_done,
            Stage::Stopped => t.stopped,
            Stage::Error => t.error,
        }
    }

    fn refresh_note(&mut self) {
        if self.site.trim().is_empty() {
            self.note = self.lang.t().hint.to_string();
        } else {
            self.analyse();
        }
    }

    fn push_log(&mut self, text: impl Into<String>, level: Level) {
        self.log.push((text.into(), level));
        if self.log.len() > 500 {
            self.log.remove(0);
        }
    }

    fn analyse(&mut self) {
        let t = self.lang.t();
        match sanitize_site(&self.site) {
            Some((host, note)) => {
                let why = match note {
                    Note::Plain => "plain",
                    Note::Layout => "keyboard layout",
                    Note::Idn => "IDN -> punycode",
                };
                let entry = paths::url_to_local_path(&format!("https://{host}/"));
                self.note = format!("→ {host}  ({why})   {entry}");
            }
            None => self.note = t.bad_address.to_string(),
        }
    }

    fn start(&mut self) {
        if self.running {
            return;
        }
        let t = self.lang.t();
        let Some((host, _)) = sanitize_site(&self.site) else {
            self.push_log(t.bad_address, Level::Err);
            return;
        };
        if self.out.trim().is_empty() {
            self.push_log(t.need_out, Level::Err);
            return;
        }
        let out = PathBuf::from(self.out.trim());
        let Some(mut engine) = Engine::new(&host, &out, 16, 0) else {
            self.push_log(t.bad_address, Level::Err);
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
        self.stage = Stage::Inventory;
        self.status = t.preparing.to_string();
        self.push_log(format!("{} → {}", host, out.display()), Level::Ok);

        std::thread::spawn(move || {
            let result = engine
                .run()
                .map(|o| {
                    format!(
                        "скачано {} из {}, страниц {}, битых ссылок {} — отчёт: {}",
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
            let t = self.lang.t();
            self.push_log(t.stopped, Level::Warn);
        }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.rx.take() else { return };
        let mut keep = true;
        let mut changed = false;
        loop {
            match rx.try_recv() {
                Ok(Msg::Progress(p)) => {
                    changed = true;
                    if p.stage == "log" {
                        self.push_log(p.text.clone(), Level::Warn);
                        continue;
                    }
                    self.overall = p.overall;
                    self.stage = match p.stage {
                        "inventory" => Stage::Inventory,
                        "download" => Stage::Download,
                        "build" => Stage::Build,
                        "verify" => Stage::Verify,
                        "report" => Stage::Report,
                        _ => Stage::Idle,
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
                    self.eta = p.eta;
                    self.threads = p.threads;
                    for (idx, entry) in p.types.iter().enumerate() {
                        self.types[idx * 2] = entry.1;
                        self.types[idx * 2 + 1] = entry.2;
                    }
                }
                Ok(Msg::Done(result)) => {
                    changed = true;
                    keep = false;
                    self.running = false;
                    self.cancel = None;
                    match result {
                        Ok(text) => {
                            self.push_log(text, Level::Ok);
                            self.overall = 100.0;
                            self.stage = Stage::Done;
                            self.status = self.lang.t().built.to_string();
                        }
                        Err(err) if err == "cancelled" => {
                            self.stage = Stage::Stopped;
                            self.status = self.lang.t().stopped.to_string();
                        }
                        Err(err) => {
                            self.push_log(format!("{}: {err}", self.lang.t().error), Level::Err);
                            self.stage = Stage::Error;
                            self.status = self.lang.t().error.to_string();
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
        if changed {
            // финальное сообщение приходит уже после остановки прогона —
            // без этого кадра оно осталось бы непоказанным
            ctx.request_repaint();
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
        let filled = egui::Rect::from_min_size(
            rect.min,
            egui::vec2(rect.width() * frac.clamp(0.0, 1.0), rect.height()),
        );
        painter.rect_filled(filled, radius, egui::Color32::from_rgb(37, 99, 235));
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll(ui.ctx());
        if self.pending_start && !self.running {
            self.pending_start = false;
            self.start();
        }
        if self.running {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(200));
        }
        let t = self.lang.t();

        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.heading(t.heading);
                    ui.label(egui::RichText::new(t.subtitle).weak());
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                    if ui.button(self.lang.other().label()).clicked() {
                        self.lang = self.lang.other();
                        self.refresh_note();
                    }
                });
            });
            ui.add_space(10.0);

            egui::Grid::new("form")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label(t.site);
                    ui.add_enabled(
                        !self.running,
                        egui::TextEdit::singleline(&mut self.site)
                            .hint_text(t.site_ph)
                            .desired_width(f32::INFINITY),
                    );
                    ui.end_row();

                    ui.label(t.out);
                    ui.horizontal(|ui| {
                        ui.add_enabled(
                            !self.running,
                            egui::TextEdit::singleline(&mut self.out)
                                .desired_width(ui.available_width() - 190.0),
                        );
                        if ui
                            .add_enabled(!self.running, egui::Button::new(t.browse))
                            .clicked()
                        {
                            if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                                self.out = dir
                                    .join("site")
                                    .to_string_lossy()
                                    .into_owned();
                            }
                        }
                        if ui
                            .add_enabled(!self.running, egui::Button::new(t.analyse))
                            .clicked()
                        {
                            self.analyse();
                        }
                    });
                    ui.end_row();
                });

            ui.add_space(6.0);
            ui.label(egui::RichText::new(&self.note).weak());
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!self.running, egui::Button::new(t.start))
                    .clicked()
                {
                    self.start();
                }
                if ui
                    .add_enabled(self.running, egui::Button::new(t.stop))
                    .clicked()
                {
                    self.stop();
                }
                if ui.button(t.open).clicked() {
                    open_folder(&self.out);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(t.help).clicked() {
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
                ui.label(egui::RichText::new(t.log).strong());
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
                                    ui.label(egui::RichText::new(text).monospace().color(color));
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
                        ui.label(egui::RichText::new(self.stage_label()).strong());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(format!("{:.0} %", self.overall));
                        });
                    });
                    ui.add_space(6.0);
                    let full = ui.available_width();
                    bar(ui, self.overall / 100.0, 14.0, full);
                    ui.add_space(10.0);
                    let names = [t.t_html, t.t_code, t.t_img, t.t_other];
                    for (idx, name) in names.iter().enumerate() {
                        ui.horizontal(|ui| {
                            ui.add_sized([150.0, 18.0], egui::Label::new(*name));
                            let ok = self.types[idx * 2];
                            let total = self.types[idx * 2 + 1];
                            let frac = if total > 0 {
                                ok as f32 / total as f32
                            } else {
                                0.0
                            };
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
                                "{} / {} · {:.0} KB/s · threads {} · missing {} · errors {}",
                                self.done,
                                self.total,
                                self.bps / 1024.0,
                                self.threads,
                                self.missing,
                                self.errors
                            ))
                            .weak(),
                        );
                        if self.eta > 0 {
                            ui.label(egui::RichText::new(format!("~{} s", self.eta)).weak());
                        }
                    }
                    if self.bytes > 0 {
                        ui.label(
                            egui::RichText::new(format!(
                                "{:.1} MB",
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
            egui::Window::new(t.help)
                .open(&mut open)
                .default_size([640.0, 460.0])
                .show(&ctx, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        ui.label(t.help_text);
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

impl Lang {
    fn label(self) -> &'static str {
        match self {
            Lang::Ru => "EN",
            Lang::En => "RU",
        }
    }
}
