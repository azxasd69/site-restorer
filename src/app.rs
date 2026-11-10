//! Нативное окно (egui 0.37): форма, живой прогресс, журнал, RU/EN.

use crate::core::{Engine, Progress};
use crate::i18n::Lang;
use crate::paths;
use crate::sanitize::{Note, sanitize_site};
use eframe::egui;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
enum Level {
    Info,
    Ok,
    Warn,
    Err,
}

enum Msg {
    Progress(Box<Progress>),
    Done(Result<String, String>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
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
    files_per_sec: f32,
    pause: u64,
    geom: Option<crate::settings::Window>,
    last_saved: crate::settings::Settings,
    last_save_at: Instant,
    types: [usize; 8],
    rx: Option<Receiver<Msg>>,
    cancel: Option<Arc<AtomicBool>>,
    pending_start: bool,
    /// Первый кадр: отдать фокус полю адреса, чтобы Ctrl+V работал сразу.
    focus_site: bool,
    /// Момент последнего изменения поля адреса — для разбора по паузе.
    site_dirty_at: Option<f64>,
}

/// Светлая тема одним местом: границы полей, фон всплывающих окон, скругления.
fn theme() -> egui::Visuals {
    let mut v = egui::Visuals::light();
    v.panel_fill = WHITE;
    v.window_fill = WHITE;
    v.extreme_bg_color = WHITE;
    v.faint_bg_color = PAGE;
    v.window_stroke = egui::Stroke::new(1.0, BORDER);
    v.window_corner_radius = egui::CornerRadius::same(10);
    v.menu_corner_radius = egui::CornerRadius::same(8);
    v.selection.bg_fill = egui::Color32::from_rgb(191, 219, 254);
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, BORDER);
    v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, BORDER);
    v.widgets.inactive.weak_bg_fill = WHITE;
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(148, 163, 184));
    v.widgets.hovered.weak_bg_fill = egui::Color32::from_rgb(248, 250, 252);
    v.widgets.active.bg_stroke = egui::Stroke::new(1.0, ACCENT);
    v.widgets.active.weak_bg_fill = egui::Color32::from_rgb(239, 246, 255);
    v
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(theme());
        let settings = crate::settings::load();
        match settings.window {
            Some(w) => {
                // Окно могло быть сохранено на мониторе, которого больше нет:
                // возвращаем его в видимую область, иначе программа откроется «в никуда».
                let (x, y) = match cc.egui_ctx.input(|i| i.viewport().monitor_size) {
                    Some(mon) if mon.x > 400.0 && mon.y > 400.0 => (
                        w.x.clamp(-40.0, (mon.x - 160.0).max(0.0)),
                        w.y.clamp(0.0, (mon.y - 120.0).max(0.0)),
                    ),
                    _ => (w.x, w.y),
                };
                cc.egui_ctx
                    .send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(x, y)));
                cc.egui_ctx
                    .send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(w.w, w.h)));
            }
            None => {
                if let Some(mon) = cc.egui_ctx.input(|i| i.viewport().monitor_size)
                    && mon.x > 400.0
                    && mon.y > 400.0
                {
                    let size = egui::vec2(900.0, 660.0);
                    let pos = egui::pos2(
                        ((mon.x - size.x) / 2.0).max(0.0),
                        ((mon.y - 48.0 - size.y) / 2.0).max(0.0),
                    );
                    cc.egui_ctx
                        .send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
                }
            }
        }
        let lang = match std::env::var("SR_LANG").as_deref() {
            Ok("en") => Lang::En,
            Ok("ru") => Lang::Ru,
            _ if settings.lang == "en" => Lang::En,
            _ => Lang::Ru,
        };
        Self {
            site: std::env::var("SR_AUTOSTART").unwrap_or_else(|_| settings.site.clone()),
            out: std::env::var("SR_OUT").unwrap_or_else(|_| settings.out.clone()),
            note: lang.t().hint.to_string(),
            log: vec![(format!("Site Restorer {}", crate::APP_VERSION), Level::Info)],
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
            files_per_sec: 0.0,
            pause: 0,
            geom: settings.window,
            last_saved: settings,
            last_save_at: Instant::now(),
            types: [0; 8],
            rx: None,
            cancel: None,
            pending_start: std::env::var("SR_AUTOSTART")
                .map(|v| !v.is_empty())
                .unwrap_or(false),
            focus_site: true,
            site_dirty_at: None,
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
        let stamp = crate::clock::hh_mm_ss();
        self.log.push((format!("{stamp}  {}", text.into()), level));
        if self.log.len() > 500 {
            self.log.remove(0);
        }
    }

    /// Внешняя ссылка: открываем браузером по умолчанию. Неудачу показываем
    /// предупреждением в журнале — окно и прогон продолжают работать (ФТ-114).
    fn open_link(&mut self, url: &str) {
        if let Err(err) = crate::links::open(url) {
            let text = format!("{}{}", self.lang.t().link_failed, err);
            self.push_log(text, Level::Warn);
        }
    }

    fn analyse(&mut self) {
        self.normalize_field(false);
    }

    /// Разбор поля адреса. `rewrite` — заменить содержимое чистым хостом
    /// (уход фокуса, Enter); во время набора поле не трогаем.
    fn normalize_field(&mut self, rewrite: bool) {
        let t = self.lang.t();
        let raw = self.site.trim();
        if raw.is_empty() {
            self.note = t.hint.to_string();
            return;
        }
        match sanitize_site(raw) {
            Some((host, note)) => {
                self.note = self.note_line(&host, note);
                if rewrite && raw != host {
                    self.site = host;
                }
            }
            None => self.note = t.bad_address.to_string(),
        }
    }

    fn note_line(&self, host: &str, note: Note) -> String {
        let why = match note {
            Note::Plain => "plain",
            Note::Layout => "keyboard layout",
            Note::Idn => "IDN -> punycode",
        };
        let entry = paths::url_to_local_path(&format!("https://{host}/"));
        format!("→ {host}  ({why})   {entry}")
    }

    /// Вставка из буфера: чистим полезную нагрузку, разбираем сразу и
    /// подменяем поле чистым хостом — как это делала версия на Python.
    fn apply_paste(&mut self, raw: &str) {
        let t = self.lang.t();
        let cleaned = crate::sanitize::clean_payload(raw);
        let trimmed = cleaned.trim();
        if trimmed.is_empty() {
            return;
        }
        match sanitize_site(trimmed) {
            Some((host, note)) => {
                if let Ok(path) = std::env::var("SR_DIAG") {
                    use std::io::Write as _;
                    if let Ok(mut f) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)
                    {
                        let _ = writeln!(f, "site:{host}");
                    }
                }
                let noisy = raw.trim() != trimmed || trimmed != host;
                self.note = self.note_line(&host, note);
                if noisy {
                    self.push_log(format!("адрес: {} → {host}", shorten(raw)), Level::Info);
                }
                self.site = host;
            }
            None => self.note = t.bad_address.to_string(),
        }
        self.site_dirty_at = None;
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
            let _ = tx_progress.send(Msg::Progress(Box::new(p.clone())));
        });
        self.cancel = Some(engine.cancel_token());
        self.rx = Some(rx);
        self.running = true;
        self.overall = 0.0;
        self.stage = Stage::Inventory;
        self.status = t.preparing.to_string();
        self.push_log(format!("{} → {}", host, out.display()), Level::Ok);

        // сообщения ядра должны идти на том же языке, что и окно
        crate::tr::set_english(self.lang == Lang::En);
        self.persist(true);
        std::thread::spawn(move || {
            let result = engine.run().map(|o| {
                format!(
                    "скачано {} из {}, страниц {}, битых ссылок {} — отчёт: {}",
                    o.ok,
                    o.total,
                    o.stats.pages,
                    o.stats.broken,
                    o.report.display()
                )
            });
            let _ = tx.send(Msg::Done(result));
        });
    }

    /// Настройки, которые нужно запомнить между запусками.
    fn current_settings(&self) -> crate::settings::Settings {
        crate::settings::Settings {
            site: self.site.clone(),
            out: self.out.clone(),
            lang: if self.lang == Lang::En { "en" } else { "ru" }.to_string(),
            window: self.geom,
        }
    }

    /// Записать настройки, если они изменились. `force` — без задержки.
    fn persist(&mut self, force: bool) {
        if !force && self.last_save_at.elapsed() < Duration::from_millis(1500) {
            return;
        }
        let settings = self.current_settings();
        if settings == self.last_saved {
            return;
        }
        if crate::settings::save(&settings) {
            self.last_saved = settings;
            self.last_save_at = Instant::now();
        }
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
                        self.push_log(p.text.clone(), Level::Info);
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
                    self.files_per_sec = p.files_per_sec;
                    self.pause = p.pause;
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
                            self.eta = 0;
                            self.status = self.lang.t().built.to_string();
                        }
                        Err(err) if err == "cancelled" => {
                            self.stage = Stage::Stopped;
                            self.eta = 0;
                            self.status = self.lang.t().stopped.to_string();
                        }
                        Err(err) => {
                            self.push_log(format!("{}: {err}", self.lang.t().error), Level::Err);
                            self.stage = Stage::Error;
                            self.eta = 0;
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

/// Полоса прогресса: рисуем сами, чтобы она растягивалась по ширине.
fn bar(ui: &mut egui::Ui, frac: f32, height: f32, width: f32) {
    let width = width.min(ui.available_width()).max(24.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let radius = egui::CornerRadius::same((height / 2.0) as u8);
    let painter = ui.painter();
    painter.rect_filled(rect, radius, TRACK);
    if frac > 0.0 {
        let filled = egui::Rect::from_min_size(
            rect.min,
            egui::vec2(rect.width() * frac.clamp(0.0, 1.0), rect.height()),
        );
        painter.rect_filled(filled, radius, egui::Color32::from_rgb(37, 99, 235));
    }
}

// ---------- оформление ----------
const WHITE: egui::Color32 = egui::Color32::from_rgb(255, 255, 255);
const PAGE: egui::Color32 = egui::Color32::from_rgb(244, 246, 250);
const CARD: egui::Color32 = egui::Color32::WHITE;
const TILE: egui::Color32 = egui::Color32::from_rgb(247, 249, 252);
const BORDER: egui::Color32 = egui::Color32::from_rgb(226, 232, 240);
const TRACK: egui::Color32 = egui::Color32::from_rgb(226, 232, 240);
const TEXT: egui::Color32 = egui::Color32::from_rgb(15, 23, 42);
const MUTED: egui::Color32 = egui::Color32::from_rgb(100, 116, 139);
const ACCENT: egui::Color32 = egui::Color32::from_rgb(37, 99, 235);
const ACCENT_SOFT: egui::Color32 = egui::Color32::from_rgb(219, 234, 254);
const OK: egui::Color32 = egui::Color32::from_rgb(22, 163, 74);
const WARN: egui::Color32 = egui::Color32::from_rgb(217, 119, 6);
const ERR: egui::Color32 = egui::Color32::from_rgb(220, 38, 38);
const DARK: egui::Color32 = egui::Color32::from_rgb(15, 23, 42);

/// Шаг конвейера, подсвеченный в «степпере».
fn step_index(stage: Stage) -> Option<usize> {
    match stage {
        Stage::Inventory => Some(0),
        Stage::Download => Some(1),
        Stage::Build => Some(2),
        Stage::Verify => Some(3),
        Stage::Report => Some(4),
        _ => None,
    }
}

fn tile(ui: &mut egui::Ui, width: f32, label: &str, value: String, tone: egui::Color32) {
    egui::Frame::new()
        .fill(TILE)
        .stroke(egui::Stroke::new(1.0, BORDER))
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(12, 7))
        .show(ui, |ui| {
            ui.set_width(width - 26.0);
            ui.vertical(|ui| {
                ui.label(egui::RichText::new(label).size(11.5).color(MUTED));
                ui.add_space(2.0);
                ui.label(egui::RichText::new(value).size(15.0).strong().color(tone));
            });
        });
}

/// Кнопка с явным весом: главная (заливка), вторичная (контур), тихая (текст).
fn action_button(
    ui: &mut egui::Ui,
    label: &str,
    kind: u8,
    enabled: bool,
    width: f32,
    tone: egui::Color32,
) -> bool {
    let text = match kind {
        0 => egui::RichText::new(label)
            .size(13.5)
            .strong()
            .color(if enabled { WHITE } else { MUTED }),
        1 => egui::RichText::new(label)
            .size(13.5)
            .color(if enabled { tone } else { MUTED }),
        _ => egui::RichText::new(label).size(13.0).color(if enabled {
            MUTED
        } else {
            egui::Color32::from_rgb(170, 180, 195)
        }),
    };
    let mut button = egui::Button::new(text)
        .min_size(egui::vec2(width, 30.0))
        .corner_radius(7);
    button = match kind {
        0 => button
            .fill(if enabled {
                tone
            } else {
                egui::Color32::from_rgb(226, 232, 240)
            })
            .stroke(egui::Stroke::NONE),
        1 => button
            .fill(WHITE)
            .stroke(egui::Stroke::new(1.0, if enabled { tone } else { BORDER })),
        _ => button.fill(WHITE).stroke(egui::Stroke::new(1.0, BORDER)),
    };
    let resp = ui.add(button);
    if enabled { resp.clicked() } else { false }
}

fn stage_chip(ui: &mut egui::Ui, text: &str, state: u8) {
    let (fill, fg) = match state {
        2 => (ACCENT, WHITE),
        1 => (ACCENT_SOFT, ACCENT),
        _ => (egui::Color32::from_rgb(241, 245, 249), MUTED),
    };
    egui::Frame::new()
        .fill(fill)
        .corner_radius(6)
        .inner_margin(egui::Margin::symmetric(9, 4))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).size(11.5).color(fg));
        });
}

impl eframe::App for App {
    /// Настройки пишутся с задержкой 1,5 с: при закрытии окна её надо снять,
    /// иначе последний сдвиг окна или набранный адрес потеряются.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.persist(true);
    }

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
        ui.style_mut().spacing.item_spacing = egui::vec2(8.0, 8.0);
        // На Windows встроенные быстрые клавиши буфера отключены: читаем и
        // пишем сами и подсовываем egui те же события, что дал бы winit.
        #[cfg(windows)]
        {
            let (paste, copy, cut) = ui.input(|i| {
                let cmd = i.modifiers.ctrl || i.modifiers.command;
                let shift = i.modifiers.shift;
                let paste = (cmd && !shift && i.key_pressed(egui::Key::V))
                    || (cmd && shift && i.key_pressed(egui::Key::V))
                    || (shift && !cmd && i.key_pressed(egui::Key::Insert));
                let copy = cmd && !shift && i.key_pressed(egui::Key::C);
                let cut = cmd && !shift && i.key_pressed(egui::Key::X);
                (paste, copy, cut)
            });
            if paste && let Some(text) = crate::clip::read_text() {
                ui.ctx()
                    .input_mut(|i| i.events.push(egui::Event::Paste(text)));
            }
            if copy {
                ui.ctx().input_mut(|i| i.events.push(egui::Event::Copy));
            }
            if cut {
                ui.ctx().input_mut(|i| i.events.push(egui::Event::Cut));
            }
        }

        let diag = std::env::var("SR_DIAG").ok();
        // Вставка в этом кадре: Ctrl+V, Shift+Insert, средняя кнопка (X11/Wayland).
        let pasted_now = ui.input(|i| {
            i.events.iter().find_map(|event| match event {
                egui::Event::Paste(text) => Some(text.clone()),
                egui::Event::MiddleClickPaste { text, .. } => Some(text.clone()),
                _ => None,
            })
        });

        // ---------- шапка и форма ----------
        egui::Panel::top("header")
            .frame(egui::Frame::new().fill(WHITE).inner_margin(egui::Margin {
                left: 20,
                right: 20,
                top: 12,
                bottom: 10,
            }))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new(t.heading)
                                .size(20.0)
                                .strong()
                                .color(TEXT),
                        );
                        ui.add_space(2.0);
                        ui.label(egui::RichText::new(t.subtitle).size(12.5).color(MUTED));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                        let other = self.lang.other();
                        let resp = ui.add(
                            egui::Button::new(
                                egui::RichText::new(self.lang.label())
                                    .size(12.0)
                                    .color(MUTED),
                            )
                            .fill(WHITE)
                            .stroke(egui::Stroke::new(1.0, BORDER))
                            .corner_radius(7)
                            .min_size(egui::vec2(44.0, 30.0)),
                        );
                        if resp.on_hover_text(t.lang_hint).clicked() {
                            self.lang = other;
                            self.refresh_note();
                        }
                        ui.add_space(6.0);
                        if action_button(ui, t.help, 2, true, 78.0, MUTED) {
                            self.show_help = true;
                        }
                    });
                });
                ui.add_space(10.0);

                egui::Grid::new("form")
                    .num_columns(2)
                    .spacing([14.0, 10.0])
                    .min_col_width(112.0)
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new(t.site).size(12.5).color(MUTED));
                        let site_resp = ui.add_enabled(
                            !self.running,
                            egui::TextEdit::singleline(&mut self.site)
                                .hint_text(t.site_ph)
                                .frame(
                                    egui::Frame::new()
                                        .fill(WHITE)
                                        .stroke(egui::Stroke::new(1.0, BORDER))
                                        .corner_radius(7)
                                        .inner_margin(egui::Margin::symmetric(9, 6)),
                                )
                                .desired_width(f32::INFINITY),
                        );
                        // Ctrl+V сразу после запуска должен попадать в поле, а не «в никуда».
                        if self.focus_site {
                            site_resp.request_focus();
                            self.focus_site = false;
                        }
                        if site_resp.changed() {
                            self.site_dirty_at = Some(ui.input(|i| i.time));
                        }
                        if let Some(text) = pasted_now.clone()
                            && site_resp.has_focus()
                        {
                            self.apply_paste(&text);
                        }
                        #[cfg(not(windows))]
                        if site_resp.has_focus() {
                            let wants_paste = ui.input(|i| {
                                let cmd = i.modifiers.ctrl || i.modifiers.command;
                                let ctrl_shift_v =
                                    cmd && i.modifiers.shift && i.key_pressed(egui::Key::V);
                                // на Windows Shift+Insert уже обрабатывает egui-winit
                                let shift_insert = !cfg!(target_os = "windows")
                                    && i.modifiers.shift
                                    && i.key_pressed(egui::Key::Insert);
                                ctrl_shift_v || shift_insert
                            });
                            if wants_paste {
                                ui.ctx()
                                    .send_viewport_cmd(egui::ViewportCommand::RequestPaste);
                            }
                        }
                        if site_resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            site_resp.surrender_focus();
                        }
                        if site_resp.lost_focus() {
                            let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                            self.normalize_field(true);
                            if enter && !self.running {
                                self.start();
                            }
                        }
                        ui.end_row();

                        ui.label(egui::RichText::new(t.out).size(12.5).color(MUTED));
                        ui.horizontal(|ui| {
                            let buttons_w = 190.0;
                            ui.add_enabled(
                                !self.running,
                                egui::TextEdit::singleline(&mut self.out)
                                    .frame(
                                        egui::Frame::new()
                                            .fill(WHITE)
                                            .stroke(egui::Stroke::new(1.0, BORDER))
                                            .corner_radius(7)
                                            .inner_margin(egui::Margin::symmetric(9, 6)),
                                    )
                                    .desired_width((ui.available_width() - buttons_w).max(120.0)),
                            );
                            if action_button(ui, t.browse, 1, !self.running, 82.0, MUTED)
                                && let Some(dir) = crate::pick::pick_folder()
                            {
                                self.out = std::path::PathBuf::from(dir)
                                    .join("site")
                                    .to_string_lossy()
                                    .into_owned();
                            }
                            if action_button(ui, t.analyse, 1, !self.running, 96.0, MUTED) {
                                self.analyse();
                            }
                        });
                        ui.end_row();
                    });

                ui.add_space(4.0);
                let note_color = if self.note.starts_with('→') {
                    OK
                } else if self.note.starts_with('⚠') {
                    WARN
                } else {
                    MUTED
                };
                ui.label(egui::RichText::new(&self.note).size(11.5).color(note_color));
                ui.add_space(10.0);

                ui.horizontal(|ui| {
                    if action_button(ui, t.start, 0, !self.running, 112.0, ACCENT) {
                        self.start();
                    }
                    if action_button(ui, t.stop, 1, self.running, 108.0, ERR) {
                        self.stop();
                    }
                    ui.add_space(2.0);
                    if action_button(ui, t.open, 2, true, 116.0, MUTED) {
                        open_folder(&self.out);
                    }
                });
                let band = ui.max_rect().expand2(egui::vec2(20.0, 14.0));
                ui.painter().hline(
                    band.x_range(),
                    band.bottom(),
                    egui::Stroke::new(1.0, BORDER),
                );
            });

        // ---------- нижняя полоса со ссылками ----------
        // Панели-«прилипалы» разбирают края окна в порядке объявления, поэтому
        // полоса объявлена до журнала: иначе она заняла бы место над ним.
        egui::Panel::bottom("footer")
            .resizable(false)
            .exact_size(24.0)
            .frame(egui::Frame::new().fill(WHITE))
            .show(ui, |ui| {
                let rect = ui.max_rect();
                ui.painter()
                    .hline(rect.x_range(), rect.top(), egui::Stroke::new(1.0, BORDER));
                ui.style_mut().visuals.hyperlink_color = ACCENT;
                ui.horizontal_centered(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(20.0);
                        let thanks = ui
                            .add(egui::Link::new(
                                egui::RichText::new(t.footer_thanks).size(11.5),
                            ))
                            .on_hover_text(t.footer_thanks_hint);
                        if thanks.clicked() {
                            self.open_link(crate::links::SUPPORT);
                        }
                        ui.label(egui::RichText::new("·").size(11.5).color(MUTED));
                        let site = ui
                            .add(egui::Link::new(
                                egui::RichText::new(t.footer_site).size(11.5),
                            ))
                            .on_hover_text(t.footer_site_hint);
                        if site.clicked() {
                            self.open_link(crate::links::SITE);
                        }
                    });
                });
            });

        // ---------- журнал ----------
        egui::Panel::bottom("log")
            .resizable(true)
            .frame(egui::Frame::new().fill(WHITE).inner_margin(egui::Margin {
                left: 20,
                right: 20,
                top: 10,
                bottom: 12,
            }))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(t.log).size(12.5).strong().color(TEXT));
                    ui.label(
                        egui::RichText::new(format!("{}", self.log.len()))
                            .size(11.0)
                            .color(MUTED),
                    );
                });
                ui.add_space(4.0);
                let box_w = ui.available_width();
                egui::Frame::new()
                    .fill(DARK)
                    .corner_radius(8)
                    .inner_margin(egui::Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        ui.set_max_width((box_w - 20.0).max(80.0));
                        ui.set_min_height(58.0);
                        egui::ScrollArea::vertical()
                            .stick_to_bottom(true)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                for (text, level) in &self.log {
                                    let color = match level {
                                        Level::Info => egui::Color32::from_rgb(203, 213, 225),
                                        Level::Ok => egui::Color32::from_rgb(134, 239, 172),
                                        Level::Warn => egui::Color32::from_rgb(253, 224, 71),
                                        Level::Err => egui::Color32::from_rgb(252, 165, 165),
                                    };
                                    let line = ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(format!("•  {text}"))
                                                .monospace()
                                                .size(11.5)
                                                .color(color),
                                        )
                                        .truncate(),
                                    );
                                    line.on_hover_text(text);
                                }
                            });
                    });
            });
        // ---------- прогресс ----------
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(PAGE).inner_margin(egui::Margin {
                left: 20,
                right: 20,
                top: 16,
                bottom: 16,
            }))
            .show(ui, |ui| {
                egui::Frame::new()
                    .fill(CARD)
                    .stroke(egui::Stroke::new(1.0, BORDER))
                    .corner_radius(10)
                    .inner_margin(egui::Margin::same(14))
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.style_mut().spacing.item_spacing.y = 6.0;
                                ui.horizontal(|ui| {
                                    let tone = match self.stage {
                                        Stage::Done => OK,
                                        Stage::Error => ERR,
                                        Stage::Stopped => WARN,
                                        _ => ACCENT,
                                    };
                                    let (rect, _) = ui.allocate_exact_size(
                                        egui::vec2(9.0, 16.0),
                                        egui::Sense::hover(),
                                    );
                                    ui.painter().circle_filled(rect.center(), 4.0, tone);
                                    ui.label(
                                        egui::RichText::new(self.stage_label())
                                            .size(14.5)
                                            .strong()
                                            .color(TEXT),
                                    );
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if self.eta > 0 && self.running {
                                                ui.label(
                                                    egui::RichText::new(format!(
                                                        "{} {} s",
                                                        t.eta_hint, self.eta
                                                    ))
                                                    .size(11.0)
                                                    .color(MUTED),
                                                );
                                                ui.add_space(8.0);
                                            }
                                            ui.label(
                                                egui::RichText::new(format!(
                                                    "{:.0} %",
                                                    self.overall
                                                ))
                                                .size(17.0)
                                                .strong()
                                                .color(ACCENT),
                                            );
                                        },
                                    );
                                });
                                ui.add_space(10.0);
                                bar(ui, self.overall / 100.0, 8.0, ui.available_width());
                                ui.add_space(8.0);

                                let steps = [t.s_inv, t.s_dl, t.s_build, t.s_verify, t.s_report];
                                let active = step_index(self.stage);
                                ui.horizontal(|ui| {
                                    for (idx, name) in steps.iter().enumerate() {
                                        let state = match active {
                                            Some(a) if idx < a => 1,
                                            Some(a) if idx == a => 2,
                                            _ if self.stage == Stage::Done => 1,
                                            None if idx == 0 && self.stage == Stage::Idle => 1,
                                            _ => 0,
                                        };
                                        stage_chip(ui, name, state);
                                    }
                                });
                                ui.add_space(7.0);

                                if self.total > 0 || self.running {
                                    let gap = 10.0;
                                    let width =
                                        ((ui.available_width() - gap * 3.0) / 4.0).max(80.0);
                                    ui.horizontal(|ui| {
                                        tile(
                                            ui,
                                            width,
                                            t.k_loaded,
                                            format!("{} / {}", self.done, self.total),
                                            TEXT,
                                        );
                                        tile(
                                            ui,
                                            width,
                                            t.k_speed,
                                            if self.bps >= 1_048_576.0 {
                                                format!("{:.1} MB/s", self.bps / 1_048_576.0)
                                            } else {
                                                format!("{:.0} KB/s", self.bps / 1024.0)
                                            },
                                            TEXT,
                                        );
                                        tile(
                                            ui,
                                            width,
                                            t.k_threads,
                                            format!("{}", self.threads),
                                            TEXT,
                                        );
                                        tile(
                                            ui,
                                            width,
                                            t.k_issues,
                                            format!("{} / {}", self.missing, self.errors),
                                            if self.errors > 0 { ERR } else { TEXT },
                                        );
                                    });
                                    ui.add_space(4.0);
                                    ui.horizontal(|ui| {
                                        let mut text = format!(
                                            "{:.1} {}   ·   {:.1} MB",
                                            self.files_per_sec,
                                            t.k_rate,
                                            self.bytes as f64 / 1_048_576.0
                                        );
                                        if self.pause > 0 {
                                            text.push_str(&format!(
                                                "   ·   {} {}",
                                                t.k_pause, self.pause
                                            ));
                                        }
                                        ui.label(egui::RichText::new(text).size(11.5).color(MUTED));
                                    });
                                    ui.add_space(7.0);
                                    ui.separator();
                                    ui.add_space(6.0);

                                    let names = [t.t_html, t.t_code, t.t_img, t.t_other];
                                    let mut halves = self.types;
                                    ui.columns(2, |cols| {
                                        for (idx, name) in names.iter().enumerate() {
                                            let col = &mut cols[idx % 2];
                                            col.horizontal(|ui| {
                                                ui.add_sized(
                                                    [92.0, 15.0],
                                                    egui::Label::new(
                                                        egui::RichText::new(*name)
                                                            .size(11.5)
                                                            .color(MUTED),
                                                    ),
                                                );
                                                let ok = halves[idx * 2];
                                                let total = halves[idx * 2 + 1];
                                                let frac = if total > 0 {
                                                    ok as f32 / total as f32
                                                } else {
                                                    0.0
                                                };
                                                let width = (ui.available_width() - 56.0).max(40.0);
                                                bar(ui, frac, 5.0, width);
                                                ui.with_layout(
                                                    egui::Layout::right_to_left(
                                                        egui::Align::Center,
                                                    ),
                                                    |ui| {
                                                        ui.label(
                                                            egui::RichText::new(format!(
                                                                "{ok} / {total}"
                                                            ))
                                                            .monospace()
                                                            .size(11.0)
                                                            .color(MUTED),
                                                        );
                                                    },
                                                );
                                            });
                                        }
                                    });
                                    let _ = &mut halves;
                                } else {
                                    let width = ui.available_width();
                                    let height = (ui.available_height() - 6.0).clamp(58.0, 190.0);
                                    let (rect, _) = ui.allocate_exact_size(
                                        egui::vec2(width, height),
                                        egui::Sense::hover(),
                                    );
                                    let painter = ui.painter();
                                    let compact = height < 84.0;
                                    let title = painter.layout(
                                        t.empty_title.to_owned(),
                                        egui::FontId::proportional(14.0),
                                        TEXT,
                                        rect.width() - 24.0,
                                    );
                                    let note = if compact {
                                        None
                                    } else {
                                        Some(painter.layout(
                                            t.empty_note.to_owned(),
                                            egui::FontId::proportional(11.5),
                                            MUTED,
                                            rect.width() - 40.0,
                                        ))
                                    };
                                    let mut stack = 24.0 + 10.0 + title.size().y;
                                    if let Some(note) = &note {
                                        stack += 6.0 + note.size().y;
                                    }
                                    let top = rect.center().y - stack / 2.0;
                                    let icon_y = top + 12.0;
                                    painter.circle_stroke(
                                        egui::pos2(rect.center().x, icon_y),
                                        12.0,
                                        egui::Stroke::new(
                                            1.5,
                                            egui::Color32::from_rgb(203, 213, 225),
                                        ),
                                    );
                                    painter.circle_filled(
                                        egui::pos2(rect.center().x, icon_y),
                                        2.5,
                                        egui::Color32::from_rgb(203, 213, 225),
                                    );
                                    let mut y = icon_y + 22.0;
                                    let title_w = title.size().x;
                                    let title_h = title.size().y;
                                    painter.galley(
                                        egui::pos2(rect.center().x - title_w / 2.0, y),
                                        title,
                                        TEXT,
                                    );
                                    y += title_h + 6.0;
                                    if let Some(note) = note {
                                        painter.galley(
                                            egui::pos2(rect.center().x - note.size().x / 2.0, y),
                                            note,
                                            MUTED,
                                        );
                                    }
                                }
                            });
                    });
            });

        if let Some(path) = &diag {
            let mut lines: Vec<String> = Vec::new();
            ui.input(|i| {
                for event in &i.events {
                    match event {
                        egui::Event::Paste(text) => {
                            lines.push(format!("paste:{}", text.chars().count()))
                        }
                        egui::Event::Key {
                            key,
                            pressed: true,
                            modifiers,
                            ..
                        } => lines.push(format!(
                            "key:{key:?} ctrl={} shift={}",
                            modifiers.ctrl, modifiers.shift
                        )),
                        egui::Event::Text(text) => {
                            lines.push(format!("text:{}", text.chars().count()))
                        }
                        _ => {}
                    }
                }
            });
            if !lines.is_empty()
                && let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
            {
                use std::io::Write as _;
                for line in lines {
                    let _ = writeln!(f, "{line}");
                }
            }
        }
        // Копирование: забираем текст, который подготовил egui, и пишем сами —
        // встроенный путь на Windows иногда не доходит до буфера.
        #[cfg(windows)]
        {
            let copied: Vec<String> = ui.ctx().output_mut(|o| {
                o.commands
                    .iter()
                    .filter_map(|cmd| match cmd {
                        egui::OutputCommand::CopyText(text) => Some(text.clone()),
                        _ => None,
                    })
                    .collect()
            });
            for text in copied {
                crate::clip::write_text(&text);
            }
        }

        // Разбор ввода по паузе: подсказка обновляется сама, поле не переписываем.
        if let Some(t0) = self.site_dirty_at {
            let now = ui.input(|i| i.time);
            if now - t0 >= 0.4 {
                self.site_dirty_at = None;
                self.normalize_field(false);
            } else {
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(120));
            }
        }

        if self.show_help {
            let ctx = ui.ctx().clone();
            let mut open = self.show_help;
            egui::Window::new(t.help)
                .open(&mut open)
                .default_size([620.0, 440.0])
                .min_width(420.0)
                .show(&ctx, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        egui::Frame::new()
                            .fill(TILE)
                            .corner_radius(8)
                            .inner_margin(egui::Margin::same(12))
                            .show(ui, |ui| {
                                ui.label(
                                    egui::RichText::new(t.help_text)
                                        .monospace()
                                        .size(11.5)
                                        .color(TEXT),
                                );
                            });
                    });
                    ui.add_space(8.0);
                    if ui.button(t.help_reset).clicked() {
                        let _ = crate::settings::reset();
                        self.site.clear();
                        self.out = crate::settings::default_out();
                        self.lang = Lang::Ru;
                        self.geom = None;
                        self.last_saved = crate::settings::Settings::default();
                        self.note = self.lang.t().hint.to_string();
                        self.push_log(t.help_reset_done, Level::Info);
                    }
                });
            self.show_help = open;
        }

        // Геометрия окна: берём фактические размеры кадра и запоминаем.
        if let Some(rect) = ui.ctx().input(|i| i.viewport().inner_rect)
            && rect.width() >= 480.0
            && rect.height() >= 360.0
            && let Some(pos) = ui.ctx().input(|i| i.viewport().outer_rect).map(|r| r.min)
        {
            self.geom = Some(crate::settings::Window {
                x: pos.x,
                y: pos.y,
                w: rect.width(),
                h: rect.height(),
            });
        }
        self.persist(false);
    }
}

/// Короткий вид вставленного текста для журнала.
fn shorten(text: &str) -> String {
    let line: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let line = line.trim();
    if line.chars().count() <= 60 {
        line.to_string()
    } else {
        let head: String = line.chars().take(60).collect();
        format!("{head}…")
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
