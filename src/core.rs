//! Ядро: инвентаризация архива, параллельная загрузка с AIMD, сборка оффлайн-копии,
//! проверка ссылок и отчёт.

use crate::paths;
use crate::sanitize::sanitize_site;

use rayon::prelude::*;
use regex_lite::Regex;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub const UA: &str =
    "SiteRestorer/2.0 (offline mirror tool; respectful crawler; contact: local user)";
const MIN_THREADS: usize = 2;
const MAX_THREADS: usize = 24;
const MAX_PAGES: usize = 200;
const PAGE_LIMIT: usize = 20_000;
/// Основной адрес архива: сначала шифрованный канал, открытый — запасной.
/// Часть сетей блокирует порт 80, часть — только 443 (ОШ-2 в docs/DEFECTS.md).
const ARCHIVE_PRIMARY: &str = "https://web.archive.org";
const ARCHIVE_FALLBACK: &str = "http://web.archive.org";
/// Сколько попыток даётся на обращение к индексу архива.
const ARCHIVE_TRIES: u32 = 5;
const ERROR_MARKERS: [&[u8]; 3] = [
    b"This page is not available",
    b"doesn't have that page archived",
    b"Wayback Machine has not archived",
];

/// Один адрес из индекса архива.
#[derive(Clone, Debug)]
pub struct Item {
    pub url: String,
    pub ts: String,
    /// Исходный адрес из индекса: нужен, чтобы повторить запрос конкретного снимка.
    pub orig: String,
    /// Тип содержимого из индекса.
    pub mime: String,
    pub path: String,
    pub kind: &'static str,
}

/// Снимок прогресса для UI и CLI.
#[derive(Clone, Debug, Default)]
pub struct Progress {
    pub stage: &'static str,
    pub overall: f32,
    pub text: String,
    pub done: usize,
    pub total: usize,
    pub ok: usize,
    pub missing: usize,
    pub errors: usize,
    pub bytes: u64,
    pub bps: f64,
    pub files_per_sec: f32,
    /// Остаток общей паузы в секундах (0 — паузы нет).
    pub pause: u64,
    pub threads: usize,
    pub eta: u64,
    pub types: [(&'static str, usize, usize); 4],
}

/// Ответ архива: код, тело, `Retry-After`, `Content-Type`.
type Fetched = (u16, Vec<u8>, Option<f64>, Option<String>);

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub pages: usize,
    pub refs: usize,
    pub broken: usize,
    /// Файлы под именем страницы, содержимое которых страницей не является
    /// (картинка, PDF): в число страниц они не входят (ОШ-5).
    pub foreign: usize,
}

pub struct Outcome {
    pub ok: usize,
    pub total: usize,
    pub report: PathBuf,
    pub site: PathBuf,
    pub stats: Stats,
    /// Пары «скачано, всего» по типам: HTML, CSS/JS, IMG, прочее.
    pub types: [usize; 8],
    /// Файлов в готовой копии (не адресов: разные адреса могут вести на один файл).
    pub site_files: usize,
    /// Файлов в зеркале: всё скачанное, включая то, на что не ссылаются страницы.
    pub mirror_files: usize,
    /// Уникальных путей в инвентаре: столько файлов максимум может быть в зеркале.
    pub unique_paths: usize,
}

pub type Cb = Arc<dyn Fn(&Progress) + Send + Sync>;

// ---------------------------------------------------------------- gate
struct GateState {
    limit: usize,
    inflight: usize,
}

/// Динамический ограничитель конкурентности: лимит меняется на ходу (AIMD).
struct Gate {
    state: Mutex<GateState>,
    cv: Condvar,
}

impl Gate {
    fn new(limit: usize) -> Self {
        Self {
            state: Mutex::new(GateState {
                limit: limit.clamp(1, MAX_THREADS),
                inflight: 0,
            }),
            cv: Condvar::new(),
        }
    }

    fn acquire(&self) {
        let mut st = self.state.lock().unwrap();
        while st.inflight >= st.limit {
            st = self.cv.wait(st).unwrap();
        }
        st.inflight += 1;
    }

    fn release(&self) {
        let mut st = self.state.lock().unwrap();
        st.inflight = st.inflight.saturating_sub(1);
        self.cv.notify_one();
    }

    fn limit(&self) -> usize {
        self.state.lock().unwrap().limit
    }

    fn set_limit(&self, n: usize) {
        let mut st = self.state.lock().unwrap();
        st.limit = n.clamp(1, MAX_THREADS);
        self.cv.notify_all();
    }
}

// ---------------------------------------------------------------- counters
#[derive(Default)]
struct Counters {
    done: AtomicUsize,
    /// Сколько файлов уже было на диске к началу загрузки: скорость считается от них,
    /// иначе при докачке «файлов/с» показывал бы тысячи.
    baseline: AtomicUsize,
    ok: AtomicUsize,
    missing: AtomicUsize,
    errors: AtomicUsize,
    bytes: AtomicU64,
    win_bytes: AtomicU64,
    win_files: AtomicUsize,
    win_429: AtomicUsize,
    bps: Mutex<f64>,
}

#[derive(Default, Clone, Copy)]
struct TypeCounts {
    html: [usize; 2],
    code: [usize; 2],
    img: [usize; 2],
    other: [usize; 2],
}

impl TypeCounts {
    fn slot(&mut self, kind: &str) -> &mut [usize; 2] {
        match kind {
            "html" => &mut self.html,
            "code" => &mut self.code,
            "img" => &mut self.img,
            _ => &mut self.other,
        }
    }

    fn as_array(&self) -> [(&'static str, usize, usize); 4] {
        [
            ("html", self.html[0], self.html[1]),
            ("code", self.code[0], self.code[1]),
            ("img", self.img[0], self.img[1]),
            ("other", self.other[0], self.other[1]),
        ]
    }
}

/// Состояние, разделяемое воркерами.
struct Shared {
    agent: ureq::Agent,
    agent_archive: String,
    /// Запасной адрес архива; переключение после сетевой ошибки (ОШ-2).
    archive_alt: String,
    archive_switched: AtomicBool,
    archive_switch_logged: AtomicBool,
    mirror: PathBuf,
    gate: Gate,
    cancel: Arc<AtomicBool>,
    counters: Counters,
    types: Mutex<TypeCounts>,
    cb: Option<Cb>,
    /// Глобальная пауза после серии 429: её соблюдают все воркеры, а не только контроллер.
    pause_until: Mutex<Instant>,
    /// Минимальный интервал между запросами (наносекунды), 0 — без ограничения.
    rate_nanos: AtomicU64,
    rate_next: Mutex<Instant>,
    /// Кодировка из заголовка ответа: путь в копии -> Content-Type.
    content_types: Mutex<HashMap<String, String>>,
    /// Сколько файлов пришло не тем типом, чем обещал индекс (ОШ-5).
    foreign: AtomicUsize,
    /// Сколько примеров такого несовпадения уже попало в журнал.
    foreign_logged: AtomicUsize,
}

impl Shared {
    /// Действующий адрес архива: основной или запасной после переключения.
    fn archive_base(&self) -> &str {
        if self.archive_switched.load(Ordering::Relaxed) {
            &self.archive_alt
        } else {
            &self.agent_archive
        }
    }

    /// Переключает схему доступа к архиву после сетевой ошибки.
    /// Сообщение в журнал — один раз на прогон, чтобы не засорять его.
    fn switch_archive(&self, reason: &str) {
        let was_alt = self.archive_switched.fetch_xor(true, Ordering::Relaxed);
        if !was_alt && !self.archive_switch_logged.swap(true, Ordering::Relaxed) {
            let to = self.archive_base().to_string();
            self.log(crate::tr::pick(
                format!("архив: {reason}, переключаюсь на {to}"),
                format!("archive: {reason}, switching to {to}"),
            ));
        }
    }

    /// Считает файл, пришедший не тем типом, чем обещал индекс (ОШ-5).
    fn note_foreign(&self) {
        self.foreign.fetch_add(1, Ordering::Relaxed);
    }

    fn log(&self, msg: impl Into<String>) {
        if let Some(cb) = &self.cb {
            cb(&Progress {
                stage: "log",
                text: msg.into(),
                ..Default::default()
            });
        }
    }

    fn fetch(&self, url: &str) -> Result<Fetched, String> {
        self.wait_if_paused();
        self.rate_wait();
        let mut resp = self
            .agent
            .get(url)
            .header("User-Agent", UA)
            .header("Accept", "*/*")
            .call()
            .map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<f64>().ok());
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_string());
        let body = resp.body_mut().read_to_vec().map_err(|e| e.to_string())?;
        Ok((status, body, retry_after, content_type))
    }

    /// Сколько секунд осталось до конца общей паузы.
    fn pause_left(&self) -> u64 {
        let until = *self.pause_until.lock().unwrap();
        until.saturating_duration_since(Instant::now()).as_secs()
    }

    /// То же, что `fetch`, но тело читается порциями: пока идёт чтение,
    /// вызывается `on_progress(получено байт, число строк)`. Нужно для индекса:
    /// ответ в десятки мегабайт идёт долго, и без счётчиков полоса «замирает».
    fn fetch_stream(
        &self,
        url: &str,
        mut on_progress: impl FnMut(u64, usize),
    ) -> Result<Fetched, String> {
        self.wait_if_paused();
        self.rate_wait();
        let mut resp = self
            .agent
            .get(url)
            .header("User-Agent", UA)
            .header("Accept", "*/*")
            .call()
            .map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<f64>().ok());
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_string());
        let mut body: Vec<u8> = Vec::new();
        let mut lines = 0usize;
        let mut last = Instant::now();
        {
            let mut reader = resp.body_mut().as_reader();
            let mut chunk = [0u8; 64 * 1024];
            loop {
                if self.cancel.load(Ordering::Relaxed) {
                    // Частичный ответ нельзя отдавать как инвентарь: получился бы
                    // неполный список адресов без признака ошибки.
                    return Err("cancelled".to_string());
                }
                let read = match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(err) => return Err(err.to_string()),
                };
                let part = &chunk[..read];
                body.extend_from_slice(part);
                lines += part.iter().filter(|b| **b == b'\n').count();
                if last.elapsed() >= Duration::from_millis(200) {
                    on_progress(body.len() as u64, lines);
                    last = Instant::now();
                }
            }
        }
        on_progress(body.len() as u64, lines);
        Ok((status, body, retry_after, content_type))
    }

    /// Поставить глобальную паузу: воркеры увидят её перед следующим запросом.
    fn pause(&self, seconds: f64) {
        let until = Instant::now() + Duration::from_secs_f64(seconds.max(0.0));
        *self.pause_until.lock().unwrap() = until;
    }

    fn wait_if_paused(&self) {
        loop {
            let until = *self.pause_until.lock().unwrap();
            let now = Instant::now();
            if now >= until {
                return;
            }
            if self.cancel.load(Ordering::Relaxed) {
                return;
            }
            let left = (until - now).min(Duration::from_millis(200));
            std::thread::sleep(left);
        }
    }

    /// Лимит запросов в секунду (общий для всех воркеров).
    fn rate_wait(&self) {
        let nanos = self.rate_nanos.load(Ordering::Relaxed);
        if nanos == 0 {
            return;
        }
        let interval = Duration::from_nanos(nanos);
        let slot = {
            let mut next = self.rate_next.lock().unwrap();
            let now = Instant::now();
            let slot = if *next > now { *next } else { now };
            *next = slot + interval;
            slot
        };
        loop {
            let now = Instant::now();
            if now >= slot || self.cancel.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep((slot - now).min(Duration::from_millis(100)));
        }
    }

    fn sleep(&self, seconds: f64) {
        let end = Instant::now() + Duration::from_secs_f64(seconds.max(0.0));
        while Instant::now() < end {
            if self.cancel.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn fetch_one(&self, item: &Item) {
        if self.cancel.load(Ordering::Relaxed) {
            return;
        }
        let full = self.mirror.join(&item.path);
        if let Ok(meta) = fs::metadata(&full)
            && meta.len() > 0
        {
            self.counters.done.fetch_add(1, Ordering::Relaxed);
            self.counters.ok.fetch_add(1, Ordering::Relaxed);
            self.types.lock().unwrap().slot(item.kind)[0] += 1;
            return;
        }

        let year = &item.ts[..4.min(item.ts.len())];
        let current = 1970 + (now_secs() / 31_556_952) as usize;
        let mut tries: Vec<String> = vec![format!("/web/{}id_/{}", item.ts, item.url)];
        for y in [
            current,
            current.saturating_sub(1),
            current.saturating_sub(2),
        ] {
            let ys = y.to_string();
            if ys != year {
                tries.push(format!("/web/{ys}id_/{}", item.url));
            }
        }

        let mut ok = false;
        let mut missing = false;
        let mut size = 0usize;
        for path_q in &tries {
            if self.cancel.load(Ordering::Relaxed) {
                return;
            }
            let url = format!("{}{path_q}", self.archive_base());
            for attempt in 0..2u32 {
                let (status, body, retry_after, content_type) = match self.fetch(&url) {
                    Ok(res) => res,
                    Err(err) => {
                        self.log(crate::tr::pick(
                            format!("сеть: {err}"),
                            format!("network: {err}"),
                        ));
                        // Сетевая ошибка может означать, что закрыт порт схемы:
                        // пробуем другую схему, не бросая загрузку (ОШ-2).
                        self.switch_archive("сеть недоступна");
                        self.sleep(0.6 * (attempt + 1) as f64);
                        continue;
                    }
                };
                if status == 200 && body.len() > 20 && !is_error_page(&body) {
                    if let Some(parent) = full.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    let tmp = tmp_path(&full);
                    if fs::write(&tmp, &body).is_ok() {
                        if fs::rename(&tmp, &full).is_err() {
                            let _ = fs::copy(&tmp, &full);
                            let _ = fs::remove_file(&tmp);
                        }
                        // файл мог не пережить переименование — считаем успех только по факту
                        if let Ok(meta) = fs::metadata(&full)
                            && meta.len() > 0
                        {
                            ok = true;
                            size = body.len();
                            // Индекс обещает страницу, а приходит картинка или PDF:
                            // файл сохраняем как есть, но страницей не считаем (ОШ-5).
                            if item.kind == "html" && crate::encoding::looks_binary(&body) {
                                self.note_foreign();
                                if console_verbose()
                                    && self.foreign_logged.fetch_add(1, Ordering::Relaxed) < 3
                                {
                                    self.log(crate::tr::pick(
                                        format!("вместо страницы — двоичный файл: {}", item.path),
                                        format!("binary content behind a page path: {}", item.path),
                                    ));
                                }
                            }
                            if let Some(ct) = content_type {
                                self.content_types
                                    .lock()
                                    .unwrap()
                                    .insert(item.path.clone(), ct);
                            }
                        }
                    }
                    break;
                }
                if status == 429 {
                    self.counters.win_429.fetch_add(1, Ordering::Relaxed);
                    let wait = retry_after.unwrap_or(20.0 * (attempt + 1) as f64);
                    self.log(crate::tr::pick(
                        format!("429: пауза {wait:.0} с"),
                        format!("429: pause {wait:.0}s"),
                    ));
                    self.sleep(wait);
                    continue;
                }
                if status == 404 {
                    missing = true;
                    break;
                }
                self.sleep(0.8 * (attempt + 1) as f64);
            }
            if ok {
                break;
            }
        }

        self.counters.done.fetch_add(1, Ordering::Relaxed);
        if ok {
            self.counters.ok.fetch_add(1, Ordering::Relaxed);
            self.counters
                .bytes
                .fetch_add(size as u64, Ordering::Relaxed);
            self.counters
                .win_bytes
                .fetch_add(size as u64, Ordering::Relaxed);
            self.counters.win_files.fetch_add(1, Ordering::Relaxed);
            self.types.lock().unwrap().slot(item.kind)[0] += 1;
        } else if missing {
            self.counters.missing.fetch_add(1, Ordering::Relaxed);
        } else {
            self.counters.errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn tmp_path(full: &Path) -> PathBuf {
    let mut name = full
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string();
    name.push_str(".part");
    full.with_file_name(name)
}

// ---------------------------------------------------------------- engine
pub struct Engine {
    pub host: String,
    pub out: PathBuf,
    pub mirror: PathBuf,
    pub site_dir: PathBuf,
    pub data_dir: PathBuf,
    pub limit: usize,
    pub workers: usize,
    /// Сколько потоков отдаётся сборке и проверке (0 — по числу ядер).
    pub build_threads: usize,
    shared: Arc<Shared>,
    cb: Option<Cb>,
    started: Instant,
    items: Vec<Item>,
    inv_failed: bool,
    /// Сколько файлов пришлось перекодировать: кодировка -> число.
    enc_stats: std::sync::Mutex<HashMap<&'static str, usize>>,
    enc_logged: std::sync::atomic::AtomicUsize,
    /// Список файлов зеркала: дерево обходится один раз за прогон (было восемь обходов).
    mirror_cache: Mutex<Option<Arc<Vec<PathBuf>>>>,
    /// Список файлов готовой копии — так же кешируется.
    site_cache: Mutex<Option<Arc<Vec<PathBuf>>>>,
    /// Число страниц (html, не бинарных), посчитанное при сборке.
    pages: AtomicUsize,
    /// Файлы, пришедшие под именем страницы, но страницей не являющиеся.
    foreign_built: AtomicUsize,
    /// Число файлов копии и зеркала без повторного обхода дерева в отчёте.
    site_files: AtomicUsize,
    mirror_files: AtomicUsize,
    /// Цели ссылок, собранные при переписывании: путь файла -> ключи целей.
    /// Проверка ссылок идёт по ним, без второго чтения и повторного разбора страниц.
    ref_targets: Mutex<Vec<(String, Vec<String>)>>,
    /// Что уже собрано: повторный прогон не переписывает неизменённые файлы.
    journal: Mutex<HashMap<String, JournalEntry>>,
    /// Сколько файлов не удалось записать (для журнала работы и разбора жалоб).
    write_errors: AtomicUsize,
}

/// Запись журнала сборки: что уже переписано и какие цели ссылок получились.
/// Повторный прогон пропускает файл, если размер и время изменения в зеркале совпали.
#[derive(Clone, Default)]
struct JournalEntry {
    size: u64,
    mtime: u64,
    /// Ключи целей ссылок страницы — для проверки без повторного чтения.
    targets: Vec<String>,
    /// Файл считается страницей (html и не бинарный).
    page: bool,
    /// Под именем страницы лежит не страница (картинка, PDF): повторный прогон
    /// должен давать тот же счётчик, иначе «не страница» обнуляется.
    foreign: bool,
}

impl Engine {
    pub fn new(site: &str, out: &Path, workers: usize, limit: usize) -> Option<Self> {
        let (host, _note) = sanitize_site(site)?;
        // Каталог вывода всегда абсолютный: файловые операции Windows не умеют
        // длинный префикс \?\ на относительном пути, а очистка копии через него.
        let out = absolute_dir(out);
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(180)))
            .build();
        Some(Self {
            host,
            out: out.clone(),
            mirror: out.join("mirror"),
            site_dir: out.join("site"),
            data_dir: out.join("data"),
            limit,
            workers: workers.clamp(1, 64),
            build_threads: 0,
            shared: Arc::new(Shared {
                agent: ureq::Agent::new_with_config(config),
                agent_archive: ARCHIVE_PRIMARY.to_string(),
                archive_alt: ARCHIVE_FALLBACK.to_string(),
                archive_switched: AtomicBool::new(false),
                archive_switch_logged: AtomicBool::new(false),
                mirror: out.join("mirror"),
                gate: Gate::new(MIN_THREADS),
                cancel: Arc::new(AtomicBool::new(false)),
                counters: Counters::default(),
                types: Mutex::new(TypeCounts::default()),
                cb: None,
                pause_until: Mutex::new(Instant::now()),
                rate_nanos: AtomicU64::new(0),
                rate_next: Mutex::new(Instant::now()),
                content_types: Mutex::new(HashMap::new()),
                foreign: AtomicUsize::new(0),
                foreign_logged: AtomicUsize::new(0),
            }),
            cb: None,
            started: Instant::now(),
            items: Vec::new(),
            inv_failed: false,
            enc_stats: std::sync::Mutex::new(HashMap::new()),
            enc_logged: std::sync::atomic::AtomicUsize::new(0),
            mirror_cache: Mutex::new(None),
            site_cache: Mutex::new(None),
            pages: AtomicUsize::new(0),
            foreign_built: AtomicUsize::new(0),
            site_files: AtomicUsize::new(0),
            mirror_files: AtomicUsize::new(0),
            ref_targets: Mutex::new(Vec::new()),
            journal: Mutex::new(HashMap::new()),
            write_errors: AtomicUsize::new(0),
        })
    }

    /// Список файлов зеркала: один обход на прогон (раньше дерево обходилось восемь раз).
    fn mirror_list(&self) -> Arc<Vec<PathBuf>> {
        let mut cache = self.mirror_cache.lock().unwrap();
        if let Some(list) = cache.as_ref() {
            return Arc::clone(list);
        }
        let list = Arc::new(walk_files(&self.mirror));
        self.mirror_files.store(list.len(), Ordering::Relaxed);
        *cache = Some(Arc::clone(&list));
        list
    }

    /// Список файлов копии; сбрасывается после сборки.
    fn site_list(&self) -> Arc<Vec<PathBuf>> {
        let mut cache = self.site_cache.lock().unwrap();
        if let Some(list) = cache.as_ref() {
            return Arc::clone(list);
        }
        let list = Arc::new(walk_files(&self.site_dir));
        self.site_files.store(list.len(), Ordering::Relaxed);
        *cache = Some(Arc::clone(&list));
        list
    }

    fn reset_site_list(&self) {
        *self.site_cache.lock().unwrap() = None;
    }

    /// Сколько потоков отдаётся сборке и проверке.
    fn thread_count(&self) -> usize {
        if self.build_threads > 0 {
            return self.build_threads.clamp(1, 64);
        }
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(1, 32)
    }

    /// Выполняет работу в пуле потоков заданного размера.
    fn in_pool<R: Send>(&self, work: impl FnOnce() -> R + Send) -> R {
        match rayon::ThreadPoolBuilder::new()
            .num_threads(self.thread_count())
            .build()
        {
            Ok(pool) => pool.install(work),
            Err(_) => work(),
        }
    }

    /// Создаёт каталог один раз: раньше `create_dir_all` вызывался на каждый файл.
    fn ensure_dir(&self, cache: &Mutex<HashSet<PathBuf>>, dir: &Path) {
        if cache.lock().unwrap().contains(dir) {
            return;
        }
        // Захват один: между проверкой и вставкой другой поток уже мог создать каталог.
        if fs::create_dir_all(dir).is_ok() {
            cache.lock().unwrap().insert(dir.to_path_buf());
        }
    }

    /// Пишет в журнал первые ошибки записи: раньше они терялись, а отчёт
    /// показывал несуществующие файлы как собранные.
    fn log_write_error(&self, rel: &str, err: &std::io::Error) {
        if self.write_errors.fetch_add(1, Ordering::Relaxed) < 10 {
            self.shared.log(crate::tr::pick(
                format!("не удалось записать {rel}: {err}"),
                format!("cannot write {rel}: {err}"),
            ));
        }
    }

    /// Сколько потоков отдавать сборке и проверке: 0 — по числу ядер.
    pub fn set_build_threads(&mut self, threads: usize) {
        self.build_threads = threads;
    }

    pub fn set_progress_handler(&mut self, cb: impl Fn(&Progress) + Send + Sync + 'static) {
        let shared: Cb = Arc::new(cb);
        self.cb = Some(Arc::clone(&shared));
        // пересоздаём Shared с обработчиком (он нужен воркерам для журнала),
        // сохраняя уже настроенные паузу и лимит скорости
        let paused = *self.shared.pause_until.lock().unwrap();
        let rate = self.shared.rate_nanos.load(Ordering::Relaxed);
        let next = *self.shared.rate_next.lock().unwrap();
        let ct = self.shared.content_types.lock().unwrap().clone();
        self.shared = Arc::new(Shared {
            agent: self.shared.agent.clone(),
            agent_archive: self.shared.agent_archive.clone(),
            archive_alt: self.shared.archive_alt.clone(),
            archive_switched: AtomicBool::new(self.shared.archive_switched.load(Ordering::Relaxed)),
            archive_switch_logged: AtomicBool::new(
                self.shared.archive_switch_logged.load(Ordering::Relaxed),
            ),
            mirror: self.shared.mirror.clone(),
            gate: Gate::new(MIN_THREADS),
            cancel: Arc::clone(&self.shared.cancel),
            counters: Counters::default(),
            types: Mutex::new(TypeCounts::default()),
            cb: Some(shared),
            pause_until: Mutex::new(paused),
            rate_nanos: AtomicU64::new(rate),
            rate_next: Mutex::new(next),
            content_types: Mutex::new(ct),
            foreign: AtomicUsize::new(0),
            foreign_logged: AtomicUsize::new(0),
        });
    }

    /// Заголовки ответов (кодировки) пригодятся при сборке: meta в документе
    /// может отсутствовать, а заголовок архива — врать.
    fn content_types_path(&self) -> PathBuf {
        self.data_dir.join("content-types.tsv")
    }

    fn load_content_types(&self) {
        let path = self.content_types_path();
        let Ok(text) = fs::read_to_string(&path) else {
            return;
        };
        let mut map = self.shared.content_types.lock().unwrap();
        for line in text.lines() {
            if let Some((rel, ct)) = line.split_once('\t') {
                map.insert(rel.to_string(), ct.to_string());
            }
        }
    }

    fn save_content_types(&self) {
        let map = self.shared.content_types.lock().unwrap().clone();
        if map.is_empty() {
            return;
        }
        let mut out = String::new();
        for (rel, ct) in map {
            out.push_str(&rel);
            out.push('\t');
            out.push_str(&ct.replace(['\t', '\n', '\r'], " "));
            out.push('\n');
        }
        let _ = fs::create_dir_all(&self.data_dir);
        let _ = fs::write(self.content_types_path(), out);
    }

    /// Ограничение скорости: запросов в секунду (0 — без ограничения).
    pub fn set_rate(&mut self, rate: f64) {
        let nanos = if rate > 0.0 {
            (1_000_000_000.0 / rate) as u64
        } else {
            0
        };
        self.shared.rate_nanos.store(nanos, Ordering::Relaxed);
    }

    #[allow(dead_code)] // публичный API для внешних обвязок
    pub fn cancel_token(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shared.cancel)
    }

    #[allow(dead_code)] // публичный API: используется тестами и внешними вызовами
    pub fn cancel(&self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
    }

    fn emit(&self, stage: &'static str, overall: f32, text: &str, _force: bool) {
        let Some(cb) = &self.cb else { return };
        let counters = &self.shared.counters;
        let done = counters.done.load(Ordering::Relaxed);
        let total = self.items.len();
        let elapsed = self.started.elapsed().as_secs_f64().max(0.001);
        let bps = *counters.bps.lock().unwrap();
        let eta = if done > 0 && total > done {
            ((total - done) as f64 / (done as f64 / elapsed)) as u64
        } else {
            0
        };
        let types = self.shared.types.lock().unwrap().as_array();
        cb(&Progress {
            stage,
            overall,
            text: text.to_string(),
            done,
            total,
            ok: counters.ok.load(Ordering::Relaxed),
            missing: counters.missing.load(Ordering::Relaxed),
            errors: counters.errors.load(Ordering::Relaxed),
            bytes: counters.bytes.load(Ordering::Relaxed),
            bps,
            files_per_sec: (done.saturating_sub(counters.baseline.load(Ordering::Relaxed)) as f64
                / elapsed) as f32,
            pause: self.shared.pause_left(),
            threads: self.shared.gate.limit(),
            eta,
            types,
        });
    }

    // ------------------------------------------------------------ inventory
    fn cdx_query(&self, extra: &str) -> String {
        format!(
            "{}/cdx/search/cdx?url={}&matchType=domain&{}",
            self.shared.archive_base(),
            urlencode(&self.host),
            extra
        )
    }

    fn inventory(&mut self) -> Result<(), String> {
        self.emit(
            "inventory",
            2.0,
            crate::tr::t("оценка объёма архива", "estimating archive size"),
            true,
        );
        let mut pages = 1usize;
        match self
            .shared
            .fetch(&self.cdx_query("showNumPages=true&output=json"))
        {
            Ok((200, body, _, _)) => {
                if let Some(n) = parse_pages(&String::from_utf8_lossy(&body)) {
                    pages = n;
                }
            }
            Ok((code, _, _, _)) => self
                .shared
                .log(format!("CDX: оценка недоступна (HTTP {code})")),
            Err(err) => self.shared.log(crate::tr::pick(
                format!("CDX: оценка — ошибка {err}"),
                format!("CDX: estimate failed: {err}"),
            )),
        }

        // Быстрый запрос выполняется ВСЕГДА: постраничный режим (`page=N`) у
        // web.archive.org поддерживается не для всех сайтов и бывает пустым,
        // а запрос с limit=50000 отдаёт те же данные.
        let mut rows: Vec<Vec<String>> = Vec::new();
        let query = self.cdx_query(
            "output=json&fl=timestamp,original,mimetype,statuscode&filter=statuscode:200&collapse=urlkey&limit=50000",
        );
        self.emit(
            "inventory",
            4.0,
            crate::tr::t("получение списка адресов", "fetching the address list"),
            true,
        );
        // Архив регулярно отвечает 429/503 на большие выборки: несколько попыток
        // с нарастающей паузой дешевле, чем уход в постраничный режим, который у части
        // сайтов вообще не поддерживается. Сетевая ошибка значит, что порт схемы может
        // быть закрыт: тогда переключаем схему и пробуем снова (ОШ-2, ОШ-3).
        for attempt in 0..ARCHIVE_TRIES {
            let result = {
                let started = Instant::now();
                let mut on_index = |bytes: u64, lines: usize| {
                    let secs = started.elapsed().as_secs_f64();
                    let overall = 4.0 + 4.0 * (1.0 - (-secs / 15.0).exp()) as f32;
                    self.emit(
                        "inventory",
                        overall,
                        &crate::tr::pick(
                            format!(
                                "{} · записей ~{} · {secs:.0} с",
                                size_text(bytes),
                                lines.saturating_sub(1)
                            ),
                            format!(
                                "{} · records ~{} · {secs:.0} s",
                                size_text(bytes),
                                lines.saturating_sub(1)
                            ),
                        ),
                        false,
                    );
                };
                self.shared.fetch_stream(&query, &mut on_index)
            };
            match result {
                Ok((200, body, _, _)) => {
                    rows = parse_rows(&body);
                    if !rows.is_empty() {
                        break;
                    }
                }
                Ok((code, _, retry, _)) => {
                    self.shared.log(crate::tr::pick(
                        format!("CDX: список — HTTP {code}"),
                        format!("CDX: list — HTTP {code}"),
                    ));
                    if code == 429 || code >= 500 {
                        let wait = retry.unwrap_or(10.0 * (attempt + 1) as f64);
                        self.shared.sleep(wait);
                        continue;
                    }
                    // Код ошибки вместо пустого ответа — отказ индекса, а не «нет страниц».
                    if code != 200 {
                        self.inv_failed = true;
                    }
                    break;
                }
                Err(err) => {
                    // Остановка пользователем — не ошибка списка: выходим сразу,
                    // иначе журнал заполняется «ошибка cancelled» и прогон ждёт паузу.
                    if self.shared.cancel.load(Ordering::Relaxed) {
                        return Err("cancelled".to_string());
                    }
                    self.shared.log(crate::tr::pick(
                        format!("CDX: список — ошибка {err}"),
                        format!("CDX: list failed: {err}"),
                    ));
                    self.shared.switch_archive("список недоступен");
                    self.inv_failed = true;
                    let wait = 5.0 * (1u32 << attempt.min(4)) as f64;
                    self.shared.log(crate::tr::pick(
                        format!(
                            "CDX: пауза {wait:.0} с, попытка {}/{ARCHIVE_TRIES}",
                            attempt + 2
                        ),
                        format!(
                            "CDX: pause {wait:.0}s, attempt {}/{ARCHIVE_TRIES}",
                            attempt + 2
                        ),
                    ));
                    self.shared.sleep(wait);
                    continue;
                }
            }
            break;
        }
        if rows.is_empty() {
            self.shared.log(crate::tr::t(
                "CDX: быстрый запрос пуст — постраничный обход",
                "CDX: quick query empty — switching to paging",
            ));
            pages = pages.max(2);
        } else if rows.len() >= 50_000 {
            self.shared.log(crate::tr::t(
                "CDX: список обрезан на 50000 адресах",
                "CDX: address list truncated at 50000",
            ));
        }
        if rows.is_empty() && pages > 1 {
            let max_pages = pages.min(MAX_PAGES);
            let mut page = 0usize;
            let mut attempts = 0u32;
            while page < max_pages {
                if self.shared.cancel.load(Ordering::Relaxed) {
                    return Err("cancelled".to_string());
                }
                let query = self.cdx_query(&format!(
                    "output=json&fl=timestamp,original,mimetype,statuscode&filter=statuscode:200&collapse=urlkey&limit={PAGE_LIMIT}&page={page}"
                ));
                let page_no = page;
                self.emit(
                    "inventory",
                    4.0 + 4.0 * page_no as f32 / max_pages as f32,
                    &crate::tr::pick(
                        format!("CDX: страница {} из {}", page_no + 1, max_pages),
                        format!("CDX: page {} of {}", page_no + 1, max_pages),
                    ),
                    true,
                );
                let result = {
                    let started = Instant::now();
                    let mut on_page = |bytes: u64, lines: usize| {
                        let secs = started.elapsed().as_secs_f64();
                        let inner = 1.0 - (-secs / 15.0).exp();
                        let overall =
                            4.0 + 4.0 * (page_no as f32 + inner as f32) / max_pages as f32;
                        self.emit(
                            "inventory",
                            overall,
                            &crate::tr::pick(
                                format!(
                                    "CDX: страница {} · {} · записей ~{} · {secs:.0} с",
                                    page_no + 1,
                                    size_text(bytes),
                                    lines.saturating_sub(1)
                                ),
                                format!(
                                    "CDX: page {} · {} · records ~{} · {secs:.0} s",
                                    page_no + 1,
                                    size_text(bytes),
                                    lines.saturating_sub(1)
                                ),
                            ),
                            false,
                        );
                    };
                    self.shared.fetch_stream(&query, &mut on_page)
                };
                match result {
                    Ok((200, body, _, _)) => {
                        attempts = 0;
                        let chunk = parse_rows(&body);
                        let got = chunk.len();
                        rows.extend(chunk);
                        page += 1;
                        if got < PAGE_LIMIT {
                            break;
                        }
                        continue;
                    }
                    Ok((code, _, retry, _)) => {
                        if code == 429 || code >= 500 {
                            attempts += 1;
                            if attempts >= 3 {
                                self.shared.log(format!(
                                    "CDX: страница {} — HTTP {code} после {attempts} попыток, останавливаюсь",
                                page + 1
                                ));
                                self.inv_failed = true;
                                break;
                            }
                            let wait = retry.unwrap_or(15.0);
                            self.shared.log(crate::tr::pick(
                                format!("CDX: HTTP {code}, пауза {wait:.0} с"),
                                format!("CDX: HTTP {code}, pause {wait:.0}s"),
                            ));
                            self.shared.sleep(wait);
                            continue;
                        }
                        self.shared.log(crate::tr::pick(
                            format!("CDX: страница {} — HTTP {code}", page + 1),
                            format!("CDX: page {} — HTTP {code}", page + 1),
                        ));
                        self.inv_failed = true;
                        break;
                    }
                    Err(err) => {
                        attempts += 1;
                        self.shared.switch_archive("страница недоступна");
                        if attempts >= 3 {
                            self.shared.log(crate::tr::pick(
                                format!("CDX: страница {} — ошибка {err}", page + 1),
                                format!("CDX: page {} failed: {err}", page + 1),
                            ));
                            self.inv_failed = true;
                            break;
                        }
                        self.shared.sleep(5.0);
                        continue;
                    }
                }
            }
        }

        let mut best: HashMap<String, (String, String, String)> = HashMap::new();
        for row in &rows {
            if row.len() < 3 {
                continue;
            }
            let ts = row[0].clone();
            let orig = row[1].clone();
            let mime = row[2].clone();
            let url = paths::normalize_url(&orig, &self.host);
            match best.get(&url) {
                Some((old_ts, _, _)) if *old_ts >= ts => {}
                _ => {
                    best.insert(url, (ts, orig, mime));
                }
            }
        }

        let mut items: Vec<Item> = best
            .into_iter()
            .map(|(url, (ts, orig, mime))| Item {
                path: paths::url_to_local_path(&url),
                kind: paths::file_type(&url),
                url,
                ts,
                orig,
                mime,
            })
            .collect();
        items.sort_by(|a, b| a.url.cmp(&b.url));
        if self.limit > 0 {
            items.truncate(self.limit);
        }
        {
            let mut types = self.shared.types.lock().unwrap();
            for item in &items {
                types.slot(item.kind)[1] += 1;
            }
        }
        fs::create_dir_all(&self.data_dir).map_err(|e| e.to_string())?;
        let mut inv = String::new();
        for item in &items {
            inv.push_str(&format!(
                "{{\"url\":\"{}\",\"ts\":\"{}\",\"orig\":\"{}\",\"mime\":\"{}\",\"path\":\"{}\",\"type\":\"{}\"}}\n",
                json_escape(&item.url),
                json_escape(&item.ts),
                json_escape(&item.orig),
                json_escape(&item.mime),
                json_escape(&item.path),
                item.kind
            ));
        }
        fs::write(self.data_dir.join("inventory.jsonl"), inv).map_err(|e| e.to_string())?;
        self.items = items;
        if self.items.is_empty() {
            return Err(if self.inv_failed {
                "archive unavailable".to_string()
            } else {
                "archive has no pages for this address".to_string()
            });
        }
        self.shared.log(crate::tr::pick(
            format!("адресов найдено: {}", self.items.len()),
            format!("addresses found: {}", self.items.len()),
        ));
        // Отказ индекса (503 или ограничение частоты) выглядит как домен из одной страницы:
        // копия формально успешна, но пуста. Говорим об этом прямо.
        if self.items.len() == 1 && self.inv_failed {
            self.shared.log(crate::tr::pick(
                "архив отдал только главную страницу — вероятен отказ индекса (503 или предел частоты), повторите прогон позже"
                    .to_string(),
                "the archive returned the front page only — the index likely refused (503 or rate limit); retry later"
                    .to_string(),
            ));
        }
        self.emit("inventory", 8.0, "", true);
        Ok(())
    }

    // ------------------------------------------------------------ download
    fn order_interleaved(items: &[Item]) -> Vec<Item> {
        let mut buckets: HashMap<&'static str, Vec<Item>> = HashMap::new();
        for item in items {
            buckets.entry(item.kind).or_default().push(item.clone());
        }
        for list in buckets.values_mut() {
            list.sort_by(|a, b| a.url.cmp(&b.url));
        }
        let order = ["html", "code", "img", "other"];
        let mut out = Vec::with_capacity(items.len());
        let mut idx = 0;
        loop {
            let mut added = false;
            for kind in order {
                if let Some(item) = buckets.get(kind).and_then(|l| l.get(idx)) {
                    out.push(item.clone());
                    added = true;
                }
            }
            if !added {
                break;
            }
            idx += 1;
        }
        out
    }

    fn download(&mut self) -> Result<(), String> {
        fs::create_dir_all(&self.mirror).map_err(|e| e.to_string())?;
        let mut todo: Vec<Item> = Vec::new();
        let mut already = 0usize;
        for item in &self.items {
            let full = self.mirror.join(&item.path);
            match fs::metadata(&full) {
                Ok(meta) if meta.len() > 0 => {
                    already += 1;
                    self.shared.types.lock().unwrap().slot(item.kind)[0] += 1;
                }
                _ => todo.push(item.clone()),
            }
        }
        self.shared.counters.done.store(already, Ordering::Relaxed);
        self.shared.counters.ok.store(already, Ordering::Relaxed);
        self.shared
            .counters
            .baseline
            .store(already, Ordering::Relaxed);
        self.started = Instant::now();
        self.shared.log(crate::tr::pick(
            format!("уже в папке: {already}, к скачиванию: {}", todo.len()),
            format!("already on disk: {already}, to download: {}", todo.len()),
        ));
        if todo.is_empty() {
            self.emit("download", 85.0, "", true);
            return Ok(());
        }

        let queue = Arc::new(Self::order_interleaved(&todo));
        let next = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let total = self.items.len();

        // AIMD-контроллер
        let controller = {
            let shared = Arc::clone(&self.shared);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut prev_speed = 0.0f64;
                let mut bad_streak = 0usize;
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_secs(3));
                    let bytes = shared.counters.win_bytes.swap(0, Ordering::Relaxed) as f64;
                    let files = shared.counters.win_files.swap(0, Ordering::Relaxed);
                    let n429 = shared.counters.win_429.swap(0, Ordering::Relaxed);
                    let bps = bytes / 3.0;
                    *shared.counters.bps.lock().unwrap() = bps;
                    let limit = shared.gate.limit();
                    if n429 > 0 {
                        bad_streak += 1;
                        if bad_streak >= 3 {
                            shared.gate.set_limit(MIN_THREADS);
                            shared.log(crate::tr::t(
                                "много 429 — пауза 120 с, потоков: 2",
                                "many 429 — pause 120s, threads: 2",
                            ));
                            shared.pause(120.0);
                            bad_streak = 0;
                        } else {
                            shared.gate.set_limit(limit / 2);
                        }
                    } else if files > 0 && bps >= prev_speed * 0.95 {
                        shared.gate.set_limit(limit + 1);
                    } else if files > 0 && bps < prev_speed * 0.6 {
                        shared.gate.set_limit(limit.saturating_sub(1));
                    }
                    prev_speed = bps;
                }
            })
        };

        // поток прогресса
        let progress_thread = {
            let shared = Arc::clone(&self.shared);
            let stop = Arc::clone(&stop);
            let cb = self.cb.clone();
            let started = self.started;
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if let Some(cb) = &cb {
                        let done = shared.counters.done.load(Ordering::Relaxed);
                        let elapsed = started.elapsed().as_secs_f64().max(0.001);
                        let bps = *shared.counters.bps.lock().unwrap();
                        let types = shared.types.lock().unwrap().as_array();
                        cb(&Progress {
                            stage: "download",
                            overall: 10.0 + 75.0 * done as f32 / total.max(1) as f32,
                            text: crate::tr::pick(
                                format!("Скачано {done} из {total}"),
                                format!("Downloaded {done} of {total}"),
                            ),
                            done,
                            total,
                            ok: shared.counters.ok.load(Ordering::Relaxed),
                            missing: shared.counters.missing.load(Ordering::Relaxed),
                            errors: shared.counters.errors.load(Ordering::Relaxed),
                            bytes: shared.counters.bytes.load(Ordering::Relaxed),
                            bps,
                            files_per_sec: (done
                                .saturating_sub(shared.counters.baseline.load(Ordering::Relaxed))
                                as f64
                                / elapsed) as f32,
                            pause: shared.pause_left(),
                            threads: shared.gate.limit(),
                            eta: if done > 0
                                && total > done
                                && !shared.cancel.load(Ordering::Relaxed)
                            {
                                ((total - done) as f64 / (done as f64 / elapsed)) as u64
                            } else {
                                0
                            },
                            types,
                        });
                    }
                    std::thread::sleep(Duration::from_millis(300));
                }
            })
        };

        let mut handles = Vec::new();
        for _ in 0..self.workers {
            let shared = Arc::clone(&self.shared);
            let queue = Arc::clone(&queue);
            let next = Arc::clone(&next);
            handles.push(std::thread::spawn(move || {
                loop {
                    let idx = next.fetch_add(1, Ordering::Relaxed);
                    if idx >= queue.len() || shared.cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    shared.gate.acquire();
                    shared.fetch_one(&queue[idx]);
                    shared.gate.release();
                }
            }));
        }

        for handle in handles {
            let _ = handle.join();
        }
        stop.store(true, Ordering::Relaxed);
        let _ = progress_thread.join();
        let _ = controller.join();
        self.save_content_types();
        self.emit("download", 85.0, "", true);
        self.ensure_not_cancelled()
    }

    // ------------------------------------------------------------ build
    /// Собирает копию: переписывает ссылки, приводит кодировки, копирует файлы.
    ///
    /// Файлы обрабатываются параллельно; дерево зеркала обходится один раз; каталоги
    /// создаются один раз на путь; повторный прогон пропускает неизменённые файлы,
    /// беря цели ссылок из журнала сборки.
    fn build(&self) -> Result<usize, String> {
        let files = self.mirror_list();
        let index = Index::of(&files, &self.mirror);
        let total = files.len().max(1);

        let journal = self.load_journal();
        let incremental = !journal.is_empty();
        if !incremental && self.site_dir.exists() {
            // remove_dir_all спотыкается о длинные пути Windows (глубокие каталоги
            // архивных сайтов): чистим своим обходом, добавляя префикс \\?\.
            remove_tree(&self.site_dir);
        }
        fs::create_dir_all(&self.site_dir).map_err(|e| e.to_string())?;

        let attr_rx = Regex::new(
            r#"(?i)(href|src|data-src|data-bg|poster|action|data-original)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'<>`=]+))"#,
        )
        .unwrap();
        let srcset_rx =
            Regex::new(r#"(?i)(srcset|data-srcset)\s*=\s*(?:"([^"]*)"|'([^']*)')"#).unwrap();
        let css_rx = Regex::new(r#"url\(\s*["']?([^"')]+)["']?\s*\)"#).unwrap();
        let base_rx = Regex::new(r"(?i)<base\b[^>]*>").unwrap();
        let site_rx = Regex::new(&format!(
            r#"(?i)^(?:https?:)?//(?:www\.)?{}(?::\d+)?(/[^"'\s<>()\\]*)"#,
            regex_lite::escape(&self.host)
        ))
        .unwrap();

        let dirs: Mutex<HashSet<PathBuf>> = Mutex::new(HashSet::new());
        let done = AtomicUsize::new(0);
        let pages = AtomicUsize::new(0);
        let skipped = AtomicUsize::new(0);
        let fresh_journal: Mutex<HashMap<String, JournalEntry>> =
            Mutex::new(HashMap::with_capacity(files.len()));

        let results: Vec<(String, JournalEntry)> = self.in_pool(|| {
            files
                .par_iter()
                .filter_map(|src| {
                    if self.shared.cancel.load(Ordering::Relaxed) {
                        return None;
                    }
                    let rel = src
                        .strip_prefix(&self.mirror)
                        .unwrap_or(src)
                        .to_string_lossy()
                        .replace('\\', "/");
                    let dst = self
                        .site_dir
                        .join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));

                    let meta = fs::metadata(src).ok()?;
                    let size = meta.len();
                    let mtime = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);

                    // Файл уже собран и в зеркале не менялся: переписывать нечего.
                    if let Some(prev) = journal.get(&rel)
                        && prev.size == size
                        && prev.mtime == mtime
                        && dst.exists()
                    {
                        skipped.fetch_add(1, Ordering::Relaxed);
                        if prev.page {
                            pages.fetch_add(1, Ordering::Relaxed);
                        }
                        if prev.foreign {
                            self.foreign_built.fetch_add(1, Ordering::Relaxed);
                        }
                        self.bump_build(&done, total);
                        return Some((rel, prev.clone()));
                    }

                    if let Some(parent) = dst.parent() {
                        self.ensure_dir(&dirs, parent);
                    }
                    let lower = rel.to_lowercase();
                    let src_dir = rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                    let is_page = lower.ends_with(".html") || lower.ends_with(".htm");

                    if lower.ends_with(".html")
                        || lower.ends_with(".htm")
                        || lower.ends_with(".css")
                        || lower.ends_with(".js")
                    {
                        let data = match fs::read(src) {
                            Ok(d) => d,
                            Err(_) => return None,
                        };
                        if crate::encoding::looks_binary(&data) {
                            // под расширением .html в архиве бывает картинка или PDF:
                            // копируем байты как есть, иначе файл будет испорчен
                            let copied = match fs::copy(src, &dst) {
                                Ok(_) => true,
                                Err(e) => {
                                    self.log_write_error(&rel, &e);
                                    false
                                }
                            };
                            if copied && is_page {
                                self.foreign_built.fetch_add(1, Ordering::Relaxed);
                            }
                            self.bump_build(&done, total);
                            if !copied {
                                return None;
                            }
                            return Some((
                                rel.clone(),
                                JournalEntry {
                                    size,
                                    mtime,
                                    targets: Vec::new(),
                                    page: false,
                                    foreign: is_page,
                                },
                            ));
                        }
                        let ct = self.shared.content_types.lock().unwrap().get(&rel).cloned();
                        let decoded = crate::encoding::decode_document(&data, ct.as_deref());
                        if decoded.was_converted() {
                            *self
                                .enc_stats
                                .lock()
                                .unwrap()
                                .entry(decoded.label)
                                .or_insert(0) += 1;
                            if console_verbose()
                                && self.enc_logged.fetch_add(1, Ordering::Relaxed) < 3
                            {
                                self.shared.log(crate::tr::pick(
                                    format!(
                                        "кодировка: {} ({}, {}%) — {rel}",
                                        decoded.label, decoded.source, decoded.confidence
                                    ),
                                    format!(
                                        "encoding: {} ({}, {}%) — {rel}",
                                        decoded.label, decoded.source, decoded.confidence
                                    ),
                                ));
                            }
                        }

                        let mut text = decoded.text;
                        // Цели ссылок страницы: проверка обойдётся без второго чтения и разбора.
                        let mut targets: Vec<String> = Vec::new();
                        let mut collect = |raw: &str, quoted: bool| {
                            if is_page
                                && quoted
                                && let Some(key) = self.ref_key(raw, src_dir)
                            {
                                targets.push(key);
                            }
                        };
                        if lower.ends_with(".js") {
                            text = site_rx
                                .replace_all(&text, |caps: &regex_lite::Captures| {
                                    caps[1].to_string()
                                })
                                .into_owned();
                        } else {
                            text = attr_rx
                                .replace_all(&text, |caps: &regex_lite::Captures| {
                                    let attr = &caps[1];
                                    let (value, quote) = if let Some(v) = caps.get(2) {
                                        (v.as_str(), "\"")
                                    } else if let Some(v) = caps.get(3) {
                                        (v.as_str(), "'")
                                    } else {
                                        (caps.get(4).map(|m| m.as_str()).unwrap_or(""), "\"")
                                    };
                                    let new_value =
                                        self.rewrite_ref(value, src_dir, &index, &site_rx);
                                    // Считаем ИТОГОВУЮ ссылку: проверка ссылается на то, что
                                    // реально попало в копию (так же считала проверка в 3.3.1).
                                    collect(&new_value, caps.get(4).is_none());
                                    format!("{attr}={quote}{new_value}{quote}")
                                })
                                .into_owned();
                            text = srcset_rx
                                .replace_all(&text, |caps: &regex_lite::Captures| {
                                    let attr = &caps[1];
                                    let quote = if caps.get(2).is_some() { "\"" } else { "'" };
                                    let value = caps
                                        .get(2)
                                        .or_else(|| caps.get(3))
                                        .map(|m| m.as_str())
                                        .unwrap_or("");
                                    let items: Vec<String> = value
                                        .split(',')
                                        .filter(|s| !s.trim().is_empty())
                                        .map(|item| {
                                            let mut bits = item.split_whitespace();
                                            let url = bits.next().unwrap_or("");
                                            let rest: Vec<&str> = bits.collect();
                                            let new_url =
                                                self.rewrite_ref(url, src_dir, &index, &site_rx);
                                            if rest.is_empty() {
                                                new_url
                                            } else {
                                                format!("{new_url} {}", rest.join(" "))
                                            }
                                        })
                                        .collect();
                                    format!("{attr}={quote}{}{quote}", items.join(", "))
                                })
                                .into_owned();
                            text = css_rx
                                .replace_all(&text, |caps: &regex_lite::Captures| {
                                    let url = &caps[1];
                                    format!(
                                        "url({})",
                                        self.rewrite_ref(url, src_dir, &index, &site_rx)
                                    )
                                })
                                .into_owned();
                            text = base_rx.replace_all(&text, "").into_owned();
                            text = crate::encoding::normalize_utf8(&text);
                        }
                        // Страницу считаем записанной: иначе отчёт покажет файлы,
                        // которых на диске нет, а журнал запомнит их собранными.
                        let written = match fs::write(&dst, text.as_bytes()) {
                            Ok(()) => true,
                            Err(e) => {
                                self.log_write_error(&rel, &e);
                                false
                            }
                        };
                        if written && is_page {
                            pages.fetch_add(1, Ordering::Relaxed);
                        }
                        self.bump_build(&done, total);
                        if !written {
                            return None;
                        }
                        return Some((
                            rel.clone(),
                            JournalEntry {
                                size,
                                mtime,
                                targets,
                                page: is_page,
                                foreign: false,
                            },
                        ));
                    }

                    if let Err(e) = fs::copy(src, &dst) {
                        self.log_write_error(&rel, &e);
                        self.bump_build(&done, total);
                        return None;
                    }
                    self.bump_build(&done, total);
                    Some((
                        rel.clone(),
                        JournalEntry {
                            size,
                            mtime,
                            targets: Vec::new(),
                            page: false,
                            foreign: false,
                        },
                    ))
                })
                .collect()
        });

        self.reset_site_list();
        let mut journal_new = HashMap::with_capacity(results.len());
        let mut targets_all: Vec<(String, Vec<String>)> = Vec::with_capacity(results.len());
        for (rel, entry) in results {
            if !entry.targets.is_empty() {
                targets_all.push((rel.clone(), entry.targets.clone()));
            }
            journal_new.insert(rel, entry);
        }
        *self.ref_targets.lock().unwrap() = targets_all;
        *fresh_journal.lock().unwrap() = journal_new.clone();
        *self.journal.lock().unwrap() = journal_new;
        self.pages
            .store(pages.load(Ordering::Relaxed), Ordering::Relaxed);

        // Осиротевшие файлы прошлой копии убираем только при полном прогоне:
        // при ограничении числа адресов часть зеркала просто не скачана.
        if incremental && self.limit == 0 {
            self.drop_orphans(&files);
        }
        self.save_journal();
        self.site_files
            .store(walk_files(&self.site_dir).len(), Ordering::Relaxed);

        let summary = self.enc_summary();
        if !summary.is_empty() {
            self.shared.log(crate::tr::pick(
                format!("перекодировано в UTF-8: {summary}"),
                format!("converted to UTF-8: {summary}"),
            ));
        }
        if skipped.load(Ordering::Relaxed) > 0 {
            self.shared.log(crate::tr::pick(
                format!(
                    "сборка: без изменений {}, обновлено {}",
                    skipped.load(Ordering::Relaxed),
                    total.saturating_sub(skipped.load(Ordering::Relaxed))
                ),
                format!(
                    "build: unchanged {}, updated {}",
                    skipped.load(Ordering::Relaxed),
                    total.saturating_sub(skipped.load(Ordering::Relaxed))
                ),
            ));
        }
        Ok(total)
    }

    /// Считает прогресс сборки и сообщает его не чаще, чем раз в 25 файлов.
    fn bump_build(&self, done: &AtomicUsize, total: usize) {
        let n = done.fetch_add(1, Ordering::Relaxed) + 1;
        if n.is_multiple_of(25) || n == total {
            self.emit(
                "build",
                86.0 + 9.0 * n as f32 / total as f32,
                &format!("{n}/{total}"),
                true,
            );
        }
    }

    /// Ключ цели ссылки: нормализованный абсолютный путь без учёта регистра.
    /// Возвращает None для внешних и служебных ссылок — их не проверяют.
    fn ref_key(&self, raw: &str, src_dir: &str) -> Option<String> {
        let raw = raw.trim();
        if raw.is_empty()
            || raw.starts_with("http://")
            || raw.starts_with("https://")
            || raw.starts_with("//")
            || raw.starts_with("mailto:")
            || raw.starts_with("tel:")
            || raw.starts_with("javascript:")
            || raw.starts_with("data:")
        {
            return None;
        }
        // Совместимость с прежним подсчётом: значения с якорем в отчёте не участвовали.
        if raw.contains('#') {
            return None;
        }
        if raw
            .chars()
            .any(|c| c.is_whitespace() || "(){},;'\"*`".contains(c))
        {
            return None;
        }
        let clean = crate::sanitize::pct_decode(
            raw.split('?')
                .next()
                .unwrap_or("")
                .split('#')
                .next()
                .unwrap_or(""),
        );
        let base = if src_dir.is_empty() {
            self.site_dir.clone()
        } else {
            self.site_dir
                .join(src_dir.replace('/', std::path::MAIN_SEPARATOR_STR))
        };
        let target = base.join(
            clean
                .trim_start_matches('/')
                .replace('/', std::path::MAIN_SEPARATOR_STR),
        );
        let full = norm_key(&target.to_string_lossy().replace('\\', "/")).to_lowercase();
        // Ключ храним относительно корня копии: журнал становится переносимым
        // и не раздувается на длину пути рабочего каталога.
        let root = norm_key(&self.site_dir.to_string_lossy().replace('\\', "/")).to_lowercase();
        let key = full
            .strip_prefix(&root)
            .map(|rest| rest.trim_start_matches('/').to_string())
            .unwrap_or(full);
        Some(key)
    }

    /// Удаляет из копии файлы, которых нет в зеркале.
    fn drop_orphans(&self, files: &[PathBuf]) {
        let keep: HashSet<String> = files
            .iter()
            .filter_map(|p| {
                p.strip_prefix(&self.mirror)
                    .ok()
                    .map(|r| r.to_string_lossy().replace('\\', "/").to_lowercase())
            })
            .collect();
        for path in walk_files(&self.site_dir) {
            let rel = path
                .strip_prefix(&self.site_dir)
                .unwrap_or(path.as_path())
                .to_string_lossy()
                .replace('\\', "/")
                .to_lowercase();
            if !keep.contains(&rel) {
                let _ = fs::remove_file(long_path(&path));
            }
        }
    }

    fn journal_path(&self) -> PathBuf {
        self.data_dir.join("build.jsonl")
    }

    fn load_journal(&self) -> HashMap<String, JournalEntry> {
        let Ok(text) = fs::read_to_string(self.journal_path()) else {
            return HashMap::new();
        };
        // Метка формата: журнал другой версии программы описывает иную логику
        // переписывания, поэтому его записи не годятся — нужна полная сборка.
        let mut lines = text.lines();
        if journal_label(lines.next()).as_deref() != Some(self.journal_tag().as_str()) {
            return HashMap::new();
        }
        lines.filter_map(parse_journal_line).collect()
    }

    /// Метка журнала: формат, версия сборки и хост.
    fn journal_tag(&self) -> String {
        format!(
            "#journal {} {} {}",
            JOURNAL_FORMAT,
            env!("CARGO_PKG_VERSION"),
            self.host
        )
    }

    fn save_journal(&self) {
        let journal = self.journal.lock().unwrap();
        let mut out = String::with_capacity(journal.len() * 128);
        out.push_str(&self.journal_tag());
        out.push('\n');
        for (rel, entry) in journal.iter() {
            out.push_str("{\"path\":\"");
            out.push_str(&json_escape(rel));
            out.push_str(&format!(
                "\",\"size\":{},\"mtime\":{},\"page\":{},\"foreign\":{},\"targets\":[",
                entry.size, entry.mtime, entry.page, entry.foreign
            ));
            for (i, key) in entry.targets.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('"');
                out.push_str(&json_escape(key));
                out.push('"');
            }
            out.push_str("]}\n");
        }
        let path = self.journal_path();
        // Каталог данных может отсутствовать (сборка без загрузки) — создаём сами,
        // иначе журнал молча не сохранялся и повторный прогон собирал всё заново.
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let tmp = path.with_extension("jsonl.part");
        if fs::write(&tmp, out).is_ok() {
            let _ = fs::rename(&tmp, &path);
        }
    }

    /// «windows-1251: 120, koi8-r: 2» — для журнала и отчёта.
    /// HTML-фрагмент про перекодировку (пустая строка, если конвертировать не пришлось).
    fn report_encodings(&self) -> String {
        let summary = self.enc_summary();
        if summary.is_empty() {
            return String::new();
        }
        let label = crate::tr::pick(
            "Перекодировано в UTF-8".to_string(),
            "Converted to UTF-8".to_string(),
        );
        format!("<tr><td>{label}</td><td class=\"n\">{summary}</td></tr>")
    }

    fn enc_summary(&self) -> String {
        let stats = self.enc_stats.lock().unwrap();
        let mut pairs: Vec<(&&'static str, &usize)> = stats.iter().collect();
        pairs.sort_by(|a, b| b.1.cmp(a.1));
        pairs
            .iter()
            .map(|(label, count)| format!("{label}: {count}"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn rewrite_ref(&self, value: &str, src_dir: &str, index: &Index, site_rx: &Regex) -> String {
        let raw = value.trim();
        if raw.is_empty()
            || raw.starts_with('#')
            || raw.starts_with("mailto:")
            || raw.starts_with("tel:")
            || raw.starts_with("javascript:")
            || raw.starts_with("data:")
        {
            return value.to_string();
        }
        let absolute = if site_rx.is_match(raw) {
            Some(paths::normalize_url(raw, &self.host))
        } else if raw.starts_with('/') && !raw.starts_with("//") {
            Some(paths::normalize_url(
                &format!("https://{}{raw}", self.host),
                &self.host,
            ))
        } else {
            None
        };
        let Some(url) = absolute else {
            return value.to_string();
        };
        let rel = paths::url_to_local_path(&url);
        match index.resolve(&rel) {
            Some(target) => rel_from(src_dir, &target),
            None => value.to_string(),
        }
    }

    // ------------------------------------------------------------ verify
    /// Проверяет ссылки по множеству путей копии и целям, собранным при переписывании.
    /// Ни `stat` на каждую ссылку, ни повторного чтения страниц здесь нет.
    fn verify(&self) -> Stats {
        let files = self.site_list();
        let root = norm_key(&self.site_dir.to_string_lossy().replace('\\', "/")).to_lowercase();
        let known: HashSet<String> = files
            .iter()
            .map(|p| {
                let full = norm_key(&p.to_string_lossy().replace('\\', "/")).to_lowercase();
                full.strip_prefix(&root)
                    .map(|rest| rest.trim_start_matches('/').to_string())
                    .unwrap_or(full)
            })
            .collect();
        // Цели не копируем: на большой копии это десятки мегабайт строк.
        let targets = self.ref_targets.lock().unwrap();
        let refs = AtomicUsize::new(0);
        let broken = AtomicUsize::new(0);
        self.in_pool(|| {
            targets.par_iter().for_each(|(_rel, keys)| {
                for key in keys {
                    refs.fetch_add(1, Ordering::Relaxed);
                    let as_dir = format!("{key}/index.html");
                    if !known.contains(key) && !known.contains(&as_dir) {
                        broken.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        });
        Stats {
            pages: self.pages.load(Ordering::Relaxed),
            refs: refs.load(Ordering::Relaxed),
            broken: broken.load(Ordering::Relaxed),
            foreign: self.foreign_built.load(Ordering::Relaxed),
        }
    }

    // ------------------------------------------------------------ report
    fn report(&self, stats: Stats) -> Result<PathBuf, String> {
        if self.shared.cancel.load(Ordering::Relaxed) {
            return Err("cancelled".to_string());
        }
        let types = self.shared.types.lock().unwrap().as_array();
        let mut rows = String::new();
        for (name, ok, total) in types {
            let share = if total > 0 {
                100.0 * ok as f32 / total as f32
            } else {
                0.0
            };
            rows.push_str(&format!(
                "<tr><td>{}</td><td class='n'>{ok}</td><td class='n'>{total}</td><td class='n'>{share:.0}%</td></tr>",
                label(name)
            ));
        }
        let ok = self.shared.counters.ok.load(Ordering::Relaxed);
        let total = self.items.len();
        // объём считаем по готовой копии: при докачке счётчик сессии показал бы
        // только докачанные файлы
        let site_list = self.site_list();
        let bytes: u64 = site_list
            .iter()
            .filter_map(|f| fs::metadata(f).ok())
            .map(|m| m.len())
            .sum();
        // Адресов в инвентаре может быть больше, чем файлов: разные адреса
        // (варианты с параметрами, повторы адреса) ведут на один файл (ОШ-6).
        let site_files = site_list.len();
        let mirror_files = self.mirror_list().len();
        let unique_paths = self
            .items
            .iter()
            .map(|i| i.path.as_str())
            .collect::<HashSet<&str>>()
            .len();
        let tr2 = |ru: &str, en: &str| crate::tr::pick(ru.to_string(), en.to_string());
        let doc = REPORT_TMPL
            .replace("{{lang}}", if crate::tr::is_english() { "en" } else { "ru" })
            .replace("{{t_title}}", &tr2("Отчёт:", "Report:"))
            .replace("{{t_h1}}", &tr2("Копия сайта:", "Copy of"))
            .replace("{{t_sub}}", &tr2("Отчёт о покрытии архива", "Archive coverage report"))
            .replace("{{t_downloaded}}", &tr2("Скачано", "Downloaded"))
            .replace("{{t_coverage}}", &tr2("Покрытие", "Coverage"))
            .replace("{{t_size}}", &tr2("Объём", "Size"))
            .replace("{{t_pages}}", &tr2("Страниц", "Pages"))
            .replace("{{t_files}}", &tr2("Файлов в копии", "Files in copy"))
            .replace("{{t_mirror}}", &tr2("Файлов в зеркале", "Files in mirror"))
            .replace("{{t_unique}}", &tr2("Уникальных путей в инвентаре", "Unique paths in inventory"))
            .replace("{{t_foreign}}", &tr2("Под видом страницы не страница", "Not a page behind a page name"))
            .replace(
                "{{t_files_note}}",
                &tr2(
                    "Скачанных адресов может быть больше, чем файлов: разные адреса (варианты с параметрами) ведут на один файл. В готовую копию попадают файлы, на которые ссылаются страницы; остальные остаются в папке mirror.",
                    "There can be more downloaded addresses than files: different addresses (query variants) lead to one file. The copy gets the files that pages reference; the rest stay in the mirror folder.",
                ),
            )
            .replace("{{unit_mb}}", &tr2("МБ", "MB"))
            .replace("{{t_types}}", &tr2("По типам файлов", "By file type"))
            .replace("{{t_type}}", &tr2("Тип", "Type"))
            .replace("{{t_total}}", &tr2("Всего", "Total"))
            .replace("{{t_share}}", &tr2("Доля", "Share"))
            .replace("{{t_integrity}}", &tr2("Целостность", "Integrity"))
            .replace("{{t_refs}}", &tr2("Внутренних ссылок", "Internal links"))
            .replace("{{t_broken}}", &tr2("Битых ссылок", "Broken links"))
            .replace("{{t_broken_note}}", &tr2("(нет в копии)", "(missing from copy)"))
            .replace("{{t_404}}", &tr2("Отказов 404 при загрузке", "404 responses on download"))
            .replace("{{t_errors}}", &tr2("Ошибок загрузки", "Download errors"))
            .replace(
                "{{t_footer}}",
                &tr2(
                    "Статическая копия: корзина, вход и AJAX исходного сайта не работают. Открывайте папку site локальным веб-сервером.",
                    "Static copy: the original cart, sign-in and AJAX do not work. Serve the site folder with a local web server.",
                ),
            )
            .replace("{{host}}", &self.host)
            .replace("{{ok}}", &ok.to_string())
            .replace("{{total}}", &total.to_string())
            .replace(
                "{{pct}}",
                &format!("{:.1}", 100.0 * ok as f32 / total.max(1) as f32),
            )
            .replace("{{mb}}", &format!("{:.1}", bytes as f64 / 1_048_576.0))
            .replace("{{pages}}", &stats.pages.to_string())
            .replace("{{site_files}}", &site_files.to_string())
            .replace("{{mirror_files}}", &mirror_files.to_string())
            .replace("{{unique_paths}}", &unique_paths.to_string())
            .replace("{{foreign}}", &stats.foreign.to_string())
            .replace("{{refs}}", &stats.refs.to_string())
            .replace("{{broken}}", &stats.broken.to_string())
            .replace(
                "{{missing}}",
                &self
                    .shared
                    .counters
                    .missing
                    .load(Ordering::Relaxed)
                    .to_string(),
            )
            .replace(
                "{{errors}}",
                &self
                    .shared
                    .counters
                    .errors
                    .load(Ordering::Relaxed)
                    .to_string(),
            )
            .replace("{{rows}}", &rows)
            .replace("{{encodings}}", &self.report_encodings());
        let path = self.out.join("report.html");
        fs::write(&path, doc).map_err(|e| e.to_string())?;
        Ok(path)
    }

    // ------------------------------------------------------------ run
    /// Остановка пользователем — не ошибка: она возвращается отдельным значением,
    /// чтобы интерфейс и командная строка показали «остановлено», а не сбой.
    fn ensure_not_cancelled(&self) -> Result<(), String> {
        if self.shared.cancel.load(Ordering::Relaxed) {
            Err("cancelled".to_string())
        } else {
            Ok(())
        }
    }

    pub fn run(&mut self) -> Result<Outcome, String> {
        fs::create_dir_all(&self.out).map_err(|e| e.to_string())?;
        self.load_content_types();
        self.inventory()?;
        self.ensure_not_cancelled()?;
        self.download()?;
        self.ensure_not_cancelled()?;
        self.build()?;
        self.ensure_not_cancelled()?;
        let stats = self.verify();
        self.ensure_not_cancelled()?;
        let report = self.report(stats)?;
        self.emit("report", 100.0, crate::tr::t("готово", "done"), true);
        let mut types = [0usize; 8];
        for (i, (_, ok, total)) in self
            .shared
            .types
            .lock()
            .unwrap()
            .as_array()
            .iter()
            .enumerate()
        {
            types[i * 2] = *ok;
            types[i * 2 + 1] = *total;
        }
        Ok(Outcome {
            ok: self.shared.counters.ok.load(Ordering::Relaxed),
            total: self.items.len(),
            report,
            site: self.site_dir.clone(),
            stats,
            types,
            site_files: self.site_files.load(Ordering::Relaxed),
            mirror_files: self.mirror_files.load(Ordering::Relaxed),
            unique_paths: self
                .items
                .iter()
                .map(|i| i.path.as_str())
                .collect::<HashSet<&str>>()
                .len(),
        })
    }
}

/// Подробные сообщения о кодировках — только в CLI (в GUI журнал короткий).
fn console_verbose() -> bool {
    std::env::var("SR_QUIET").is_err()
}

fn label(kind: &str) -> &'static str {
    let english = crate::tr::is_english();
    match kind {
        "html" => "HTML",
        "code" => "CSS/JS/XML",
        "img" => {
            if english {
                "Images"
            } else {
                "Изображения"
            }
        }
        _ => {
            if english {
                "Other"
            } else {
                "Прочее"
            }
        }
    }
}

/// Экранирование для JSON-строк инвентаря.
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

/// Размер в человекочитаемом виде для сообщений о ходе индексации.
fn size_text(bytes: u64) -> String {
    let kb = bytes as f64 / 1024.0;
    if kb < 1024.0 {
        crate::tr::pick(format!("{kb:.0} КБ"), format!("{kb:.0} KB"))
    } else {
        crate::tr::pick(
            format!("{:.1} МБ", kb / 1024.0),
            format!("{:.1} MB", kb / 1024.0),
        )
    }
}

/// Прочитать инвентарь с диска. Понимает и прежний формат (адрес, путь, тип),
/// и текущий (с меткой снимка, исходным адресом и типом содержимого): файл
/// прежней версии должен читаться новой.
#[allow(dead_code)]
pub fn read_inventory(file: &Path) -> Vec<Item> {
    let Ok(text) = fs::read_to_string(file) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let url = json_field(line, "url")?;
            let path = json_field(line, "path")
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| paths::url_to_local_path(&url));
            Some(Item {
                kind: paths::file_type(&url),
                ts: json_field(line, "ts").unwrap_or_default(),
                orig: json_field(line, "orig").unwrap_or_else(|| url.clone()),
                mime: json_field(line, "mime").unwrap_or_default(),
                url,
                path,
            })
        })
        .collect()
}

/// Значение строкового поля из строки JSON: `"key":"value"`.
/// Числовое поле журнала: `"size":1997` (без кавычек вокруг значения).
fn json_number(line: &str, key: &str) -> Option<u64> {
    let needle = format!("\"{key}\":");
    let start = line.find(&needle)? + needle.len();
    let digits: String = line[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

fn json_field(line: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let start = line.find(&needle)? + needle.len();
    let mut out = String::new();
    let mut escaped = false;
    for ch in line[start..].chars() {
        if escaped {
            out.push(match ch {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                c => c,
            });
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => return Some(out),
            c => out.push(c),
        }
    }
    None
}

// ---------------------------------------------------------------- index
/// Индекс файлов копии: точные пути и эвристики для кэша CMS.
struct Index {
    paths: HashMap<String, String>,
    by_name: HashMap<String, String>,
    by_kind: HashMap<(String, String), String>,
    /// Миниатюры Bitrix: (папка iblock, имя файла) -> путь в копии.
    resized: HashMap<(String, String), String>,
    /// Скомпилированы один раз: resolve() вызывается на каждую ссылку.
    cache_rx: Regex,
    resize_rx: Regex,
    /// Результаты разрешения: ключ в нижнем регистре -> найденный путь.
    /// На типичной странице половина ссылок повторяется (меню, шапка, подвал).
    resolved: Mutex<HashMap<String, Option<String>>>,
}

impl Index {
    fn of(files: &[PathBuf], root: &Path) -> Self {
        let cache_rx =
            Regex::new(r"(?i)^bitrix/cache/(css|js)/s1/[^/]+/[^/]+/([a-z_]+)_[0-9a-f]+_v1")
                .unwrap();
        let resize_rx =
            Regex::new(r"(?i)^upload/resize_cache/iblock/([0-9a-z]+)/[^/]+/(.+)$").unwrap();
        let mut index = Self {
            paths: HashMap::with_capacity(files.len()),
            by_name: HashMap::with_capacity(files.len()),
            by_kind: HashMap::new(),
            resized: HashMap::new(),
            cache_rx: cache_rx.clone(),
            resize_rx,
            resolved: Mutex::new(HashMap::new()),
        };
        for path in files {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            index.paths.insert(rel.to_lowercase(), rel.clone());
            let base = rel.rsplit('/').next().unwrap_or(&rel).to_lowercase();
            index.by_name.entry(base).or_insert_with(|| rel.clone());
            if let Some(caps) = cache_rx.captures(&rel) {
                index.by_kind.insert(
                    (caps[1].to_lowercase(), caps[2].to_lowercase()),
                    rel.clone(),
                );
            }
            let lower_rel = rel.to_lowercase();
            if let Some(caps) = index.resize_rx.captures(&lower_rel) {
                let name = lower_rel.rsplit('/').next().unwrap_or("").to_string();
                index
                    .resized
                    .entry((caps[1].to_lowercase(), name))
                    .or_insert_with(|| rel.clone());
            }
        }
        index
    }

    fn resolve(&self, rel: &str) -> Option<String> {
        let trimmed = rel.trim_start_matches('/');
        // Точное совпадение — без аллокации на нижний регистр.
        if let Some(found) = self.paths.get(trimmed) {
            return Some(found.clone());
        }
        let key = trimmed.to_lowercase();
        if let Some(hit) = self.resolved.lock().unwrap().get(&key) {
            return hit.clone();
        }
        let found = self.resolve_slow(&key);
        self.resolved.lock().unwrap().insert(key, found.clone());
        found
    }

    /// Разрешение с эвристиками — вызывается только при промахе кеша.
    fn resolve_slow(&self, key: &str) -> Option<String> {
        if let Some(found) = self.paths.get(key) {
            return Some(found.clone());
        }
        if let Some(caps) = self.cache_rx.captures(key) {
            let slot = (caps[1].to_lowercase(), caps[2].to_lowercase());
            if let Some(found) = self.by_kind.get(&slot) {
                return Some(found.clone());
            }
        }
        if let Some(caps) = self.resize_rx.captures(key) {
            let orig = format!("upload/iblock/{}/{}", &caps[1], &caps[2]).to_lowercase();
            if let Some(found) = self.paths.get(&orig) {
                return Some(found.clone());
            }
        }
        // Обратная эвристика: страница ссылается на оригинал, а в архиве сохранена
        // только уменьшенная копия (типичная ситуация у Bitrix-каталогов).
        if let Some(rest) = key.strip_prefix("upload/iblock/")
            && let Some((folder, name)) = rest.split_once('/')
            && let Some(found) = self.resized.get(&(folder.to_string(), name.to_string()))
        {
            return Some(found.clone());
        }
        let base = key.rsplit('/').next().unwrap_or(key).to_string();
        if let Some(found) = self.by_name.get(&base) {
            let depth_src = key.matches('/').count();
            if found.matches('/').count() == depth_src {
                return Some(found.clone());
            }
        }
        None
    }
}

// ---------------------------------------------------------------- helpers
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Разбор ответа CDX (массив массивов строк) без внешних библиотек.
fn parse_rows(body: &[u8]) -> Vec<Vec<String>> {
    let text = String::from_utf8_lossy(body);
    let chars: Vec<char> = text.chars().collect();
    let mut rows = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '[' {
            i += 1;
            continue;
        }
        // Внешний массив — контейнер; строка данных начинается сразу с кавычки.
        let mut j = i + 1;
        while j < chars.len() && chars[j].is_whitespace() {
            j += 1;
        }
        if j >= chars.len() || chars[j] != '"' {
            i += 1;
            continue;
        }
        i = j;
        let mut row: Vec<String> = Vec::new();
        loop {
            while i < chars.len() && (chars[i].is_whitespace() || chars[i] == ',') {
                i += 1;
            }
            if i >= chars.len() || chars[i] == ']' {
                i += 1;
                break;
            }
            if chars[i] == '"' {
                i += 1;
                let mut value = String::new();
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 1;
                        value.push(match chars[i] {
                            'n' => '\n',
                            't' => '\t',
                            c => c,
                        });
                    } else {
                        value.push(chars[i]);
                    }
                    i += 1;
                }
                i += 1;
                row.push(value);
            } else {
                i += 1;
            }
        }
        if row.len() >= 2 && row[0] != "timestamp" {
            rows.push(row);
        }
    }
    rows
}

/// Сколько страниц в CDX: значение рядом с "numpages" (форматы ответа различаются).
fn parse_pages(text: &str) -> Option<usize> {
    let hay = if text.is_ascii() {
        text.to_ascii_lowercase()
    } else {
        text.to_string()
    };
    let mut from = 0usize;
    while let Some(pos) = hay[from..].find("numpages") {
        let tail = &text[from + pos + "numpages".len()..];
        let digits: String = tail
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if let Ok(n) = digits.parse::<usize>()
            && n > 0
        {
            return Some(n);
        }
        from += pos + "numpages".len();
    }
    // Объектный формат: {"pages": 3}. Кавычки в образце важны: без них
    // образец совпал бы с «numpages» из предыдущего формата.
    if let Some(pos) = hay.find("\"pages\"") {
        let tail = &text[pos + "\"pages\"".len()..];
        if let Some(colon) = tail.find(':') {
            let digits: String = tail[colon + 1..]
                .chars()
                .skip_while(|c| c.is_whitespace())
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(n) = digits.parse::<usize>()
                && n > 0
            {
                return Some(n);
            }
        }
    }
    let trimmed = text.trim();
    if !trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_digit()) {
        return trimmed.parse::<usize>().ok().filter(|n| *n > 0);
    }
    None
}

fn is_error_page(body: &[u8]) -> bool {
    let head = &body[..body.len().min(2000)];
    ERROR_MARKERS
        .iter()
        .any(|marker| head.windows(marker.len()).any(|w| w == *marker))
}

fn rel_from(src_dir: &str, target: &str) -> String {
    let src: Vec<&str> = src_dir.split('/').filter(|s| !s.is_empty()).collect();
    let dst: Vec<&str> = target.split('/').filter(|s| !s.is_empty()).collect();
    let mut common = 0;
    while common < src.len() && common < dst.len() && src[common] == dst[common] {
        common += 1;
    }
    let mut parts: Vec<String> = (common..src.len()).map(|_| "..".to_string()).collect();
    parts.extend(dst[common..].iter().map(|s| s.to_string()));
    let joined = parts.join("/");
    if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

fn now_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Формат журнала сборки. Меняется, когда меняется смысл записей.
const JOURNAL_FORMAT: u32 = 1;

/// Каталог вывода в абсолютном виде: относительный путь нельзя склеивать
/// с префиксом длинного пути, иначе файловые операции Windows его не находят.
fn absolute_dir(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path),
        Err(_) => path.to_path_buf(),
    }
}

/// Первая строка журнала: `#journal <формат> <версия> <хост>`.
fn journal_label(line: Option<&str>) -> Option<String> {
    let line = line?.trim();
    line.starts_with("#journal ").then(|| line.to_string())
}

/// Префикс длинного пути Windows: без него файловые операции не видят пути длиннее 260 символов.
fn long_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if cfg!(windows) && path.is_absolute() && !text.starts_with(r"\\?\") {
        PathBuf::from(format!(r"\\?\{text}"))
    } else {
        path.to_path_buf()
    }
}

/// Удаляет дерево каталогов, обходя его сам: `remove_dir_all` останавливается
/// на длинных путях и оставляет часть файлов — в копии появлялся мусор прошлой сборки.
fn remove_tree(root: &Path) {
    let mut stack = vec![root.to_path_buf()];
    let mut dirs: Vec<PathBuf> = Vec::new();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(long_path(&dir)) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(path),
                _ => {
                    let _ = fs::remove_file(long_path(&path));
                }
            }
        }
        dirs.push(dir);
    }
    for dir in dirs.into_iter().rev() {
        let _ = fs::remove_dir(long_path(&dir));
    }
}

/// Убирает из пути «.» и «..»: файловая система их разворачивает, а строковый ключ — нет.
fn norm_key(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// Разбирает строку журнала сборки: путь, размер, время изменения, страница ли, цели ссылок.
fn parse_journal_line(line: &str) -> Option<(String, JournalEntry)> {
    if line.trim().is_empty() {
        return None;
    }
    let path = json_field(line, "path")?;
    let size = json_number(line, "size")?;
    let mtime = json_number(line, "mtime")?;
    let page = line.contains("\"page\":true");
    let foreign = line.contains("\"foreign\":true");
    let mut targets = Vec::new();
    if let Some(start) = line.find("\"targets\":[") {
        let rest = &line[start + "\"targets\":[".len()..];
        if let Some(end) = rest.find(']') {
            let mut buf = String::new();
            let mut in_str = false;
            let mut chars = rest[..end].chars();
            while let Some(c) = chars.next() {
                match c {
                    '"' => {
                        if in_str {
                            targets.push(std::mem::take(&mut buf));
                        }
                        in_str = !in_str;
                    }
                    '\\' if in_str => {
                        if let Some(next) = chars.next() {
                            buf.push(next);
                        }
                    }
                    _ if in_str => buf.push(c),
                    _ => {}
                }
            }
        }
    }
    Some((
        path,
        JournalEntry {
            size,
            mtime,
            targets,
            page,
            foreign,
        },
    ))
}

fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut seen: HashSet<PathBuf> = HashSet::new();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if seen.insert(path.clone()) {
                    stack.push(path);
                }
            } else if path.is_file() && !path.to_string_lossy().ends_with(".part") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

const REPORT_TMPL: &str = r#"<!DOCTYPE html><html lang="{{lang}}"><head><meta charset="utf-8">
<title>{{t_title}} {{host}}</title>
<style>body{font:15px/1.5 "Segoe UI",Arial;background:#f7f8fa;color:#16181d;margin:0}
.w{max-width:860px;margin:0 auto;padding:36px 22px}h1{font-size:26px;margin:0 0 4px}
.s{color:#6b7280;margin-bottom:24px}.g{display:grid;grid-template-columns:repeat(auto-fit,minmax(160px,1fr));gap:12px}
.c{background:#fff;border:1px solid #e5e7eb;border-radius:10px;padding:14px 16px}
.k{font-size:12px;text-transform:uppercase;letter-spacing:.07em;color:#6b7280}
.v{font-size:24px;font-weight:650}table{width:100%;border-collapse:collapse;background:#fff;border:1px solid #e5e7eb;border-radius:10px;margin-top:14px}
th,td{padding:9px 12px;border-bottom:1px solid #eee;text-align:left}th{background:#fafbfc}
.n{text-align:right;font-variant-numeric:tabular-nums}h2{font-size:18px;margin:28px 0 8px}</style></head><body><div class="w">
<h1>{{t_h1}} {{host}}</h1><div class="s">{{t_sub}}</div>
<div class="g"><div class="c"><div class="k">{{t_downloaded}}</div><div class="v">{{ok}} / {{total}}</div></div>
<div class="c"><div class="k">{{t_coverage}}</div><div class="v">{{pct}}%</div></div>
<div class="c"><div class="k">{{t_size}}</div><div class="v">{{mb}} {{unit_mb}}</div></div>
<div class="c"><div class="k">{{t_pages}}</div><div class="v">{{pages}}</div></div>
<div class="c"><div class="k">{{t_files}}</div><div class="v">{{site_files}}</div></div></div>
<h2>{{t_types}}</h2><table><tr><th>{{t_type}}</th><th class="n">{{t_downloaded}}</th><th class="n">{{t_total}}</th><th class="n">{{t_share}}</th></tr>{{rows}}</table>
<h2>{{t_integrity}}</h2><table>
<tr><td>{{t_mirror}}</td><td class="n">{{mirror_files}}</td></tr>
<tr><td>{{t_unique}}</td><td class="n">{{unique_paths}}</td></tr>
<tr><td>{{t_foreign}}</td><td class="n">{{foreign}}</td></tr>
<tr><td>{{t_refs}}</td><td class="n">{{refs}}</td></tr>
<tr><td>{{t_broken}} <span style="color:#6b7280">{{t_broken_note}}</span></td><td class="n">{{broken}}</td></tr>
{{encodings}}
<tr><td>{{t_404}}</td><td class="n">{{missing}}</td></tr>
<tr><td>{{t_errors}}</td><td class="n">{{errors}}</td></tr></table>
<p style="color:#6b7280;margin-top:10px">{{t_files_note}}</p>
<p style="color:#6b7280;margin-top:24px">{{t_footer}}</p>
</div></body></html>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cdx_rows_skips_header_and_container() {
        let body = br#"[["timestamp","original","mimetype","statuscode"],
["20020120142510","http://example.com:80/","text/html","200"],
["20030101000000","https://example.com/a","text/html","200"]]"#;
        let rows = parse_rows(body);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], "20020120142510");
        assert_eq!(rows[0][1], "http://example.com:80/");
        assert_eq!(rows[1][1], "https://example.com/a");
    }

    #[test]
    fn parse_pages_variants() {
        // формат «пары ключ — значение»
        assert_eq!(parse_pages(r#"[["numpages"],["912"]]"#), Some(912));
        // объектный формат: раньше не распознавался (см. план, Д-07)
        assert_eq!(parse_pages(r#"{"pages": 3}"#), Some(3));
        assert_eq!(parse_pages(r#"{"pages": 41, "other": 1}"#), Some(41));
        // голое число
        assert_eq!(parse_pages("12"), Some(12));
        assert_eq!(parse_pages(""), None);
        assert_eq!(parse_pages(r#"{"pages": 0}"#), None);
    }

    #[test]
    fn inventory_reads_old_and_new_format() {
        let dir = std::env::temp_dir().join(format!("sr-inventory-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("inventory.jsonl");
        let text = concat!(
            r#"{"url":"https://example.com/a","path":"a/index.html","type":"html"}"#,
            "\n",
            r#"{"url":"https://example.com/b.png","ts":"20200101000000","orig":"http://example.com/b.png","mime":"image/png","path":"b.png","type":"img"}"#,
            "\n",
            "не json",
            "\n"
        );
        std::fs::write(&file, text).unwrap();
        let items = read_inventory(&file);
        assert_eq!(items.len(), 2, "две корректные строки, мусор отброшен");
        let first = items.iter().find(|i| i.url.ends_with("/a")).unwrap();
        assert_eq!(first.kind, "html");
        assert_eq!(first.path, "a/index.html");
        assert!(first.ts.is_empty(), "в прежнем формате метки снимка нет");
        let second = items.iter().find(|i| i.url.ends_with("/b.png")).unwrap();
        assert_eq!(second.ts, "20200101000000");
        assert_eq!(second.mime, "image/png");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_rows_handles_escapes() {
        let body = br#"[["2001","https://e.com/a\"b","text/html","200"]]"#;
        let rows = parse_rows(body);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][1], "https://e.com/a\"b");
    }
}

#[cfg(test)]
mod build_tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Язык сообщений — глобальный флаг процесса, поэтому тесты, которые его
    /// переключают, идут по очереди: иначе отчёт приходит на чужом языке.
    static LANG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn workdir(tag: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("sr-{tag}-{stamp}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn put(root: &Path, rel: &str, data: &[u8]) {
        let path = root.join("mirror").join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, data).unwrap();
    }

    fn build(root: &Path) -> Engine {
        let engine = Engine::new("example.com", root, 1, 0).unwrap();
        engine.build().unwrap();
        engine
    }

    fn read(root: &Path, rel: &str) -> String {
        fs::read_to_string(root.join("site").join(rel)).unwrap()
    }

    /// ОШ-5: под именем страницы может лежать картинка. Она остаётся в копии,
    /// но страницей не считается, и это видно в отчёте.
    #[test]
    fn binary_content_behind_page_name_is_not_a_page() {
        let root = workdir("foreign");
        put(&root, "index.html", b"<html><body>ok</body></html>");
        let png = [
            0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13, b'I', b'H', b'D', b'R',
        ];
        put(&root, "clamps/index.html", &png);
        let engine = build(&root);
        let stats = engine.verify();
        assert_eq!(stats.pages, 1, "страница должна быть одна: {stats:?}");
        assert_eq!(
            stats.foreign, 1,
            "картинка под index.html не учтена: {stats:?}"
        );
        assert!(
            root.join("site/clamps/index.html").exists(),
            "файл не должен исчезать из копии"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// ОШ-6: отчёт показывает число файлов копии и пояснение о зеркале.
    #[test]
    fn report_shows_copy_file_count() {
        let _lang = LANG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = workdir("counts");
        put(&root, "index.html", b"<html><body>a</body></html>");
        put(&root, "assets/a.css", b"body{color:#000}");
        let engine = build(&root);
        let stats = engine.verify();
        let path = engine.report(stats).unwrap();
        let doc = fs::read_to_string(path).unwrap();
        assert!(doc.contains("Файлов в копии"), "нет карточки числа файлов");
        assert!(doc.contains("Файлов в зеркале"), "нет строки зеркала");
        assert!(
            doc.contains("остаются в папке mirror"),
            "нет пояснения о зеркале"
        );
        assert!(!doc.contains("{{site_files}}"), "не подставлено значение");
        let _ = fs::remove_dir_all(&root);
    }

    /// ОШ-2: схема доступа к архиву переключается и возвращается обратно.
    #[test]
    fn archive_scheme_switches_both_ways() {
        let root = workdir("scheme");
        let engine = Engine::new("example.com", &root, 1, 0).unwrap();
        assert_eq!(engine.shared.archive_base(), "https://web.archive.org");
        engine.shared.switch_archive("тест");
        assert_eq!(engine.shared.archive_base(), "http://web.archive.org");
        engine.shared.switch_archive("тест");
        assert_eq!(engine.shared.archive_base(), "https://web.archive.org");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cp1251_page_is_converted_to_utf8() {
        let root = workdir("charset");
        let page =
            "<html><head><meta charset=\"windows-1251\"></head><body><h1>Привет</h1></body></html>";
        let mut bytes = Vec::new();
        for ch in page.chars() {
            match ch {
                'П' => bytes.push(0xCF),
                'р' => bytes.push(0xF0),
                'и' => bytes.push(0xE8),
                'в' => bytes.push(0xE2),
                'е' => bytes.push(0xE5),
                'т' => bytes.push(0xF2),
                _ => bytes.push(ch as u8),
            }
        }
        put(&root, "index.html", &bytes);
        build(&root);
        let text = read(&root, "index.html");
        assert!(text.contains("Привет"), "текст: {text}");
        assert!(!text.contains("windows-1251"), "мета осталась: {text}");
        assert!(text.contains("utf-8"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn deep_charset_gets_meta_in_head() {
        let mut body = String::from("<html><head><script>var a=1;</script>");
        body.push_str(&"<!-- filler -->".repeat(80));
        body.push_str(
            "<meta http-equiv=\"Content-Type\" content=\"text/html; charset=windows-1251\">\
             </head><body>Привет</body></html>",
        );
        let out = crate::encoding::normalize_utf8(&body);
        let meta_at = out
            .find("<meta charset=\"utf-8\">")
            .expect("meta добавлена");
        assert!(
            meta_at < 1024,
            "meta должна попасть в первые 1024 байта: {meta_at}"
        );
        assert!(out.contains("charset=utf-8"));

        let early = "<html><head><meta charset=\"utf-8\"></head><body>x</body></html>";
        assert_eq!(
            crate::encoding::normalize_utf8(early),
            early,
            "корректный документ не меняем"
        );
    }

    #[test]
    fn unquoted_attribute_is_rewritten() {
        let root = workdir("unquoted");
        put(
            &root,
            "index.html",
            b"<html><body><img src=/img/a.png><a href=/page2/>x</a></body></html>",
        );
        put(&root, "img/a.png", b"\x89PNG\r\n\x1a\n");
        put(&root, "page2/index.html", b"<html><body>p2</body></html>");
        build(&root);
        let text = read(&root, "index.html");
        assert!(text.contains("src=\"img/a.png\""), "текст: {text}");
        assert!(text.contains("href=\"page2/index.html\""), "текст: {text}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn single_quoted_attribute_keeps_quotes() {
        let root = workdir("single");
        put(
            &root,
            "index.html",
            b"<html><body><a href='/page2/'>x</a></body></html>",
        );
        put(&root, "page2/index.html", b"<html><body>p2</body></html>");
        build(&root);
        let text = read(&root, "index.html");
        assert!(text.contains("href='page2/index.html'"), "текст: {text}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn css_url_and_base_are_rewritten() {
        let root = workdir("css");
        put(
            &root,
            "page/index.html",
            b"<html><head><base href=\"https://example.com/\"><link href='/css/s.css' rel=stylesheet></head><body></body></html>",
        );
        put(&root, "css/s.css", b"body{background:url(/img/a.png)}");
        put(&root, "img/a.png", b"\x89PNG");
        build(&root);
        let html = read(&root, "page/index.html");
        assert!(!html.contains("<base"), "base не удалён: {html}");
        assert!(
            html.contains("href='../css/s.css'") || html.contains("href=\"../css/s.css\""),
            "css-ссылка: {html}"
        );
        let css = read(&root, "css/s.css");
        assert!(css.contains("url(../img/a.png)"), "css: {css}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn original_image_falls_back_to_resized_copy() {
        let root = workdir("resize");
        put(
            &root,
            "catalog/item/index.html",
            b"<html><body><img src=\"/upload/iblock/f00/pic.png\"></body></html>",
        );
        put(
            &root,
            "upload/resize_cache/iblock/f00/138_110_1/pic.png",
            b"\x89PNG\r\n\x1a\n",
        );
        build(&root);
        let text = read(&root, "catalog/item/index.html");
        assert!(
            text.contains("../../upload/resize_cache/iblock/f00/138_110_1/pic.png"),
            "текст: {text}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn report_follows_interface_language() {
        let _lang = LANG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = workdir("report-lang");
        put(
            &root,
            "index.html",
            b"<html><body><a href=\"/nope.html\">x</a></body></html>",
        );
        let engine = build(&root);
        let stats = engine.verify();
        crate::tr::set_english(true);
        let path = engine.report(stats).unwrap();
        let html = fs::read_to_string(&path).unwrap();
        crate::tr::set_english(false);
        assert!(html.contains("<html lang=\"en\""), "язык документа: {html}");
        assert!(html.contains("Coverage"), "подписи на английском");
        assert!(html.contains("By file type"));
        assert!(
            !html.contains("Покрытие"),
            "русских подписей быть не должно"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn verify_ignores_javascript_artifacts() {
        let root = workdir("jsnoise");
        put(
            &root,
            "index.html",
            b"<html><body><a href=\"/nope.html\">x</a><script>a.href=(document.location.protocol)</script></body></html>",
        );
        let engine = build(&root);
        let stats = engine.verify();
        assert_eq!(stats.refs, 1, "мусор из script не должен считаться ссылкой");
        assert_eq!(stats.broken, 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn second_build_skips_unchanged_files() {
        let root = workdir("incr");
        put(
            &root,
            "index.html",
            b"<html><body><a href=\"/a.html\">a</a></body></html>",
        );
        put(&root, "a.html", b"<html><body>ok</body></html>");
        let first = build(&root);
        let stats_first = first.verify();
        let copy = root.join("site").join("index.html");
        let before = fs::metadata(&copy).unwrap().modified().unwrap();

        // Второй прогон: зеркало не менялось, файл копии не должен переписываться.
        let second = Engine::new("example.com", &root, 1, 0).unwrap();
        second.build().unwrap();
        let after = fs::metadata(&copy).unwrap().modified().unwrap();
        assert_eq!(before, after, "неизменённый файл не переписывается");
        let stats_second = second.verify();
        assert_eq!(stats_first.refs, stats_second.refs, "ссылки те же");
        assert_eq!(stats_first.broken, stats_second.broken, "битые те же");
        assert_eq!(stats_first.pages, stats_second.pages, "страницы те же");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn foreign_counter_survives_incremental_build() {
        let root = workdir("incrforeign");
        put(
            &root,
            "index.html",
            b"<html><body><a href=\"/shot.html\">i</a></body></html>",
        );
        // под именем страницы лежит картинка — страницей она не считается
        put(
            &root,
            "shot.html",
            b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0",
        );
        let first = build(&root);
        assert_eq!(first.verify().foreign, 1, "картинка под именем страницы");
        let second = Engine::new("example.com", &root, 1, 0).unwrap();
        second.build().unwrap();
        assert_eq!(
            second.verify().foreign,
            1,
            "при повторной сборке счётчик не обнуляется"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn journal_of_another_version_forces_full_build() {
        let root = workdir("jversion");
        put(&root, "index.html", b"<html><body>ok</body></html>");
        build(&root);
        let path = root.join("data").join("build.jsonl");
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("#journal "),
            "журнал помечен форматом и версией"
        );
        // Подменяем метку: журнал чужой версии не должен приниматься.
        let broken = text.replacen("#journal 1", "#journal 0", 1);
        fs::write(&path, broken).unwrap();
        let engine = Engine::new("example.com", &root, 1, 0).unwrap();
        engine.build().expect("сборка без журнала проходит");
        let after = fs::read_to_string(&path).unwrap();
        assert!(after.starts_with(&format!("#journal 1 {}", env!("CARGO_PKG_VERSION"))));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn journal_survives_round_trip() {
        let line = concat!(
            "{\"path\":\"parts/a/index.html\",\"size\":1997,\"mtime\":1794361782,",
            "\"page\":true,\"targets\":[\"img/x.png\",\"parts/b/index.html\"]}"
        );
        let (path, entry) = parse_journal_line(line).expect("строка журнала должна разбираться");
        assert_eq!(path, "parts/a/index.html");
        assert_eq!(entry.size, 1997);
        assert_eq!(entry.mtime, 1_794_361_782);
        assert!(entry.page);
        assert_eq!(entry.targets, vec!["img/x.png", "parts/b/index.html"]);
    }

    #[test]
    fn verify_counts_single_quoted_broken_links() {
        let root = workdir("verify");
        put(
            &root,
            "index.html",
            b"<html><body><a href='/nope.html'>x</a><a href=\"/nope2.html\">y</a><a href=\"https://other.org/z\">z</a></body></html>",
        );
        let engine = build(&root);
        let stats = engine.verify();
        assert_eq!(stats.pages, 1);
        assert_eq!(stats.refs, 2, "внешние ссылки не считаются");
        assert_eq!(stats.broken, 2);
        let _ = fs::remove_dir_all(&root);
    }
}
