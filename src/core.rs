//! Ядро: инвентаризация архива, параллельная загрузка с AIMD, сборка оффлайн-копии,
//! проверка ссылок и отчёт.

use crate::paths;
use crate::sanitize::sanitize_site;

use regex_lite::Regex;
use std::collections::{HashMap, HashSet};
use std::fs;
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
    pub threads: usize,
    pub eta: u64,
    pub types: [(&'static str, usize, usize); 4],
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub pages: usize,
    pub refs: usize,
    pub broken: usize,
}

pub struct Outcome {
    pub ok: usize,
    pub total: usize,
    pub report: PathBuf,
    pub site: PathBuf,
    pub stats: Stats,
    /// Пары «скачано, всего» по типам: HTML, CSS/JS, IMG, прочее.
    pub types: [usize; 8],
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
}

impl Shared {
    fn log(&self, msg: impl Into<String>) {
        if let Some(cb) = &self.cb {
            cb(&Progress {
                stage: "log",
                text: msg.into(),
                ..Default::default()
            });
        }
    }

    fn fetch(&self, url: &str) -> Result<(u16, Vec<u8>, Option<f64>, Option<String>), String> {
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
        if let Ok(meta) = fs::metadata(&full) {
            if meta.len() > 0 {
                self.counters.done.fetch_add(1, Ordering::Relaxed);
                self.counters.ok.fetch_add(1, Ordering::Relaxed);
                self.types.lock().unwrap().slot(item.kind)[0] += 1;
                return;
            }
        }

        let year = &item.ts[..4.min(item.ts.len())];
        let current = 1970 + (now_secs() / 31_556_952) as usize;
        let mut tries: Vec<String> = vec![format!("/web/{}id_/{}", item.ts, item.url)];
        for y in [current, current.saturating_sub(1), current.saturating_sub(2)] {
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
            let url = format!("{}{path_q}", self.agent_archive);
            for attempt in 0..2u32 {
                let (status, body, retry_after, content_type) = match self.fetch(&url) {
                    Ok(res) => res,
                    Err(err) => {
                        self.log(crate::tr::pick(format!("сеть: {err}"), format!("network: {err}")));
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
                        if let Ok(meta) = fs::metadata(&full) {
                            if meta.len() > 0 {
                                ok = true;
                                size = body.len();
                                if let Some(ct) = content_type {
                                    self.content_types
                                        .lock()
                                        .unwrap()
                                        .insert(item.path.clone(), ct);
                                }
                            }
                        }
                    }
                    break;
                }
                if status == 429 {
                    self.counters.win_429.fetch_add(1, Ordering::Relaxed);
                    let wait = retry_after.unwrap_or(20.0 * (attempt + 1) as f64);
                    self.log(crate::tr::pick(format!("429: пауза {wait:.0} с"), format!("429: pause {wait:.0}s")));
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
            self.counters.bytes.fetch_add(size as u64, Ordering::Relaxed);
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
    shared: Arc<Shared>,
    cb: Option<Cb>,
    started: Instant,
    items: Vec<Item>,
    inv_failed: bool,
    /// Сколько файлов пришлось перекодировать: кодировка -> число.
    enc_stats: std::sync::Mutex<HashMap<&'static str, usize>>,
    enc_logged: std::sync::atomic::AtomicUsize,
}

impl Engine {
    pub fn new(site: &str, out: &Path, workers: usize, limit: usize) -> Option<Self> {
        let (host, _note) = sanitize_site(site)?;
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(180)))
            .build();
        Some(Self {
            host,
            out: out.to_path_buf(),
            mirror: out.join("mirror"),
            site_dir: out.join("site"),
            data_dir: out.join("data"),
            limit,
            workers: workers.clamp(1, 64),
            shared: Arc::new(Shared {
                agent: ureq::Agent::new_with_config(config),
                agent_archive: "http://web.archive.org".to_string(),
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
            }),
            cb: None,
            started: Instant::now(),
            items: Vec::new(),
            inv_failed: false,
            enc_stats: std::sync::Mutex::new(HashMap::new()),
            enc_logged: std::sync::atomic::AtomicUsize::new(0),
        })
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

    fn emit(&self, stage: &'static str, overall: f32, text: &str, force: bool) {
        let Some(cb) = &self.cb else { return };
        if !force && stage != "log" {
            // троттлинг по времени
        }
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
            threads: self.shared.gate.limit(),
            eta,
            types,
        });
    }

    // ------------------------------------------------------------ inventory
    fn cdx_query(&self, extra: &str) -> String {
        format!(
            "http://web.archive.org/cdx/search/cdx?url={}&matchType=domain&{}",
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
        match self.shared.fetch(&self.cdx_query("showNumPages=true&output=json")) {
            Ok((200, body, _, _)) => {
                if let Some(n) = parse_pages(&String::from_utf8_lossy(&body)) {
                    pages = n;
                }
            }
            Ok((code, _, _, _)) => self
                .shared
                .log(format!("CDX: оценка недоступна (HTTP {code})")),
            Err(err) => self.shared.log(crate::tr::pick(format!("CDX: оценка — ошибка {err}"), format!("CDX: estimate failed: {err}"))),
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
        // Архив регулярно отвечает 429/503 на большие выборки: три попытки
        // с паузой дешевле, чем уход в постраничный режим, который у части
        // сайтов вообще не поддерживается.
        for attempt in 0..3u32 {
            match self.shared.fetch(&query) {
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
                    break;
                }
                Err(err) => {
                    self.shared.log(crate::tr::pick(
                        format!("CDX: список — ошибка {err}"),
                        format!("CDX: list failed: {err}"),
                    ));
                    self.shared.sleep(5.0);
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
                self.emit(
                    "inventory",
                    2.0 + 5.0 * (page + 1) as f32 / max_pages as f32,
                    &crate::tr::pick(
                        format!("CDX: страница {} из {}", page + 1, max_pages),
                        format!("CDX: page {} of {}", page + 1, max_pages),
                    ),
                    true,
                );
                match self.shared.fetch(&query) {
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
                            self.shared.log(crate::tr::pick(format!("CDX: HTTP {code}, пауза {wait:.0} с"), format!("CDX: HTTP {code}, pause {wait:.0}s")));
                            self.shared.sleep(wait);
                            continue;
                        }
                        self.shared
                            .log(crate::tr::pick(
                            format!("CDX: страница {} — HTTP {code}", page + 1),
                            format!("CDX: page {} — HTTP {code}", page + 1),
                        ));
                        self.inv_failed = true;
                        break;
                    }
                    Err(err) => {
                        attempts += 1;
                        if attempts >= 3 {
                            self.shared
                                .log(crate::tr::pick(format!("CDX: страница {} — ошибка {err}", page + 1), format!("CDX: page {} failed: {err}", page + 1)));
                            self.inv_failed = true;
                            break;
                        }
                        self.shared.sleep(5.0);
                        continue;
                    }
                }
            }
        }

        let mut best: HashMap<String, (String, String)> = HashMap::new();
        for row in &rows {
            if row.len() < 3 {
                continue;
            }
            let ts = row[0].clone();
            let orig = row[1].clone();
            let url = paths::normalize_url(&orig, &self.host);
            match best.get(&url) {
                Some((old_ts, _)) if *old_ts >= ts => {}
                _ => {
                    best.insert(url, (ts, orig));
                }
            }
        }

        let mut items: Vec<Item> = best
            .into_iter()
            .map(|(url, (ts, _orig))| Item {
                path: paths::url_to_local_path(&url),
                kind: paths::file_type(&url),
                url,
                ts,
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
                "{{\"url\":\"{}\",\"path\":\"{}\",\"type\":\"{}\"}}\n",
                item.url, item.path, item.kind
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
        self.shared
            .log(crate::tr::pick(format!("адресов найдено: {}", self.items.len()), format!("addresses found: {}", self.items.len())));
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
                            text: crate::tr::pick(format!("Скачано {done} из {total}"), format!("Downloaded {done} of {total}")),
                            done,
                            total,
                            ok: shared.counters.ok.load(Ordering::Relaxed),
                            missing: shared.counters.missing.load(Ordering::Relaxed),
                            errors: shared.counters.errors.load(Ordering::Relaxed),
                            bytes: shared.counters.bytes.load(Ordering::Relaxed),
                            bps,
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
            handles.push(std::thread::spawn(move || loop {
                let idx = next.fetch_add(1, Ordering::Relaxed);
                if idx >= queue.len() || shared.cancel.load(Ordering::Relaxed) {
                    break;
                }
                shared.gate.acquire();
                shared.fetch_one(&queue[idx]);
                shared.gate.release();
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
        Ok(())
    }

    // ------------------------------------------------------------ build
    fn build(&self) -> Result<usize, String> {
        let index = Index::of(&self.mirror);
        if self.site_dir.exists() {
            let _ = fs::remove_dir_all(&self.site_dir);
        }
        fs::create_dir_all(&self.site_dir).map_err(|e| e.to_string())?;

        let files = walk_files(&self.mirror);
        let total = files.len().max(1);
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

        for (i, src) in files.iter().enumerate() {
            if self.shared.cancel.load(Ordering::Relaxed) {
                return Err("cancelled".to_string());
            }
            let rel = src
                .strip_prefix(&self.mirror)
                .unwrap_or(src)
                .to_string_lossy()
                .replace('\\', "/");
            let dst = self.site_dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
            if let Some(parent) = dst.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let lower = rel.to_lowercase();
            let src_dir = rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            if lower.ends_with(".html")
                || lower.ends_with(".htm")
                || lower.ends_with(".css")
                || lower.ends_with(".js")
            {
                let data = match fs::read(src) {
                    Ok(d) => d,
                    Err(_) => continue,
                };
                if crate::encoding::looks_binary(&data) {
                    // под расширением .html в архиве бывает картинка или PDF:
                    // копируем байты как есть, иначе файл будет испорчен
                    let _ = fs::copy(src, &dst);
                    if console_verbose()
                        && self.enc_logged.fetch_add(1, Ordering::Relaxed) < 3
                    {
                        self.shared
                            .log(crate::tr::pick(format!("двоичный файл под текстовым расширением: {rel}"), format!("binary file behind a text extension: {rel}")));
                    }
                    continue;
                }
                let ct = self
                    .shared
                    .content_types
                    .lock()
                    .unwrap()
                    .get(&rel)
                    .cloned();
                let decoded = crate::encoding::decode_document(&data, ct.as_deref());
                if decoded.was_converted() {
                    *self
                        .enc_stats
                        .lock()
                        .unwrap()
                        .entry(decoded.label)
                        .or_insert(0) += 1;
                    // в журнал — только первые несколько примеров, дальше сводка
                    if console_verbose()
                        && self.enc_logged.fetch_add(1, Ordering::Relaxed) < 3
                    {
                        self.shared.log(crate::tr::pick(
                                format!(
                                    "кодировка: {} ({}, {}%) — {}",
                                    decoded.label, decoded.source, decoded.confidence, rel
                                ),
                                format!(
                                    "encoding: {} ({}, {}%) — {}",
                                    decoded.label, decoded.source, decoded.confidence, rel
                                ),
                            ));
                    }
                }
                let mut text = decoded.text;
                if lower.ends_with(".js") {
                    text = site_rx
                        .replace_all(&text, |caps: &regex_lite::Captures| caps[1].to_string())
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
                            let new_value = self.rewrite_ref(value, src_dir, &index, &site_rx);
                            format!("{attr}={quote}{new_value}{quote}")
                        })
                        .into_owned();
                    text = srcset_rx
                        .replace_all(&text, |caps: &regex_lite::Captures| {
                            let attr = &caps[1];
                            let quote = if caps.get(2).is_some() { "\"" } else { "'" };
                            let value = caps.get(2).or_else(|| caps.get(3)).map(|m| m.as_str()).unwrap_or("");
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
                            format!("url({})", self.rewrite_ref(url, src_dir, &index, &site_rx))
                        })
                        .into_owned();
                    text = base_rx.replace_all(&text, "").into_owned();
                    text = crate::encoding::normalize_utf8(&text);
                }
                let _ = fs::write(&dst, text.as_bytes());
            } else if fs::copy(src, &dst).is_err() {
                continue;
            }
            if (i + 1) % 25 == 0 || i + 1 == total {
                self.emit(
                    "build",
                    86.0 + 9.0 * (i + 1) as f32 / total as f32,
                    &format!("{}/{}", i + 1, total),
                    true,
                );
            }
        }
        let summary = self.enc_summary();
        if !summary.is_empty() {
            self.shared.log(crate::tr::pick(format!("перекодировано в UTF-8: {summary}"), format!("converted to UTF-8: {summary}")));
        }
        Ok(total)
    }

    /// «windows-1251: 120, koi8-r: 2» — для журнала и отчёта.
    /// HTML-фрагмент про перекодировку (пустая строка, если конвертировать не пришлось).
    fn report_encodings(&self) -> String {
        let summary = self.enc_summary();
        if summary.is_empty() {
            return String::new();
        }
        format!(
            "<tr><td>Перекодировано в UTF-8</td><td class=\"n\">{summary}</td></tr>"
        )
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

    fn rewrite_ref(
        &self,
        value: &str,
        src_dir: &str,
        index: &Index,
        site_rx: &Regex,
    ) -> String {
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
            Some(paths::normalize_url(&format!("https://{}{raw}", self.host), &self.host))
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
    fn verify(&self) -> Stats {
        let ref_rx = Regex::new(
            r##"(?i)(?:href|src|data-src|poster|action)\s*=\s*(?:"([^"#]+)"|'([^'#]+)')"##,
        )
        .unwrap();
        let mut stats = Stats::default();
        for path in walk_files(&self.site_dir) {
            let name = path.to_string_lossy().to_lowercase();
            if !name.ends_with(".html") && !name.ends_with(".htm") {
                continue;
            }
            stats.pages += 1;
            let text = match fs::read_to_string(&path) {
                Ok(t) => t,
                Err(_) => continue,
            };
            for caps in ref_rx.captures_iter(&text) {
                let raw = caps
                    .get(1)
                    .or_else(|| caps.get(2))
                    .map(|m| m.as_str().trim())
                    .unwrap_or("");
                if raw.is_empty()
                    || raw.starts_with("http://")
                    || raw.starts_with("https://")
                    || raw.starts_with("//")
                    || raw.starts_with("mailto:")
                    || raw.starts_with("tel:")
                    || raw.starts_with("javascript:")
                    || raw.starts_with("data:")
                {
                    continue;
                }
                // внутри <script> регулярка ловит конструкции вида href=(...) — это не ссылки
                if raw
                    .chars()
                    .any(|c| c.is_whitespace() || "(){},;'\"*`".contains(c))
                {
                    continue;
                }
                stats.refs += 1;
                let clean = crate::sanitize::pct_decode(
                    raw.split('?').next().unwrap_or("").split('#').next().unwrap_or(""),
                );
                let target = path
                    .parent()
                    .unwrap_or(Path::new("."))
                    .join(clean.trim_start_matches('/').replace('/', std::path::MAIN_SEPARATOR_STR));
                let target = if target.is_dir() {
                    target.join("index.html")
                } else {
                    target
                };
                if !target.exists() {
                    stats.broken += 1;
                }
            }
        }
        stats
    }

    // ------------------------------------------------------------ report
    fn report(&self, stats: Stats) -> Result<PathBuf, String> {
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
        let bytes: u64 = walk_files(&self.site_dir)
            .iter()
            .filter_map(|f| fs::metadata(f).ok())
            .map(|m| m.len())
            .sum();
        let doc = REPORT_TMPL
            .replace("{{host}}", &self.host)
            .replace("{{ok}}", &ok.to_string())
            .replace("{{total}}", &total.to_string())
            .replace(
                "{{pct}}",
                &format!("{:.1}", 100.0 * ok as f32 / total.max(1) as f32),
            )
            .replace("{{mb}}", &format!("{:.1}", bytes as f64 / 1_048_576.0))
            .replace("{{pages}}", &stats.pages.to_string())
            .replace("{{refs}}", &stats.refs.to_string())
            .replace("{{broken}}", &stats.broken.to_string())
            .replace(
                "{{missing}}",
                &self.shared.counters.missing.load(Ordering::Relaxed).to_string(),
            )
            .replace(
                "{{errors}}",
                &self.shared.counters.errors.load(Ordering::Relaxed).to_string(),
            )
            .replace("{{rows}}", &rows)
            .replace("{{encodings}}", &self.report_encodings());
        let path = self.out.join("report.html");
        fs::write(&path, doc).map_err(|e| e.to_string())?;
        Ok(path)
    }

    // ------------------------------------------------------------ run
    pub fn run(&mut self) -> Result<Outcome, String> {
        fs::create_dir_all(&self.out).map_err(|e| e.to_string())?;
        self.load_content_types();
        self.inventory()?;
        self.download()?;
        self.build()?;
        let stats = self.verify();
        let report = self.report(stats)?;
        self.emit("report", 100.0, crate::tr::t("готово", "done"), true);
        let mut types = [0usize; 8];
        for (i, (_, ok, total)) in self.shared.types.lock().unwrap().as_array().iter().enumerate() {
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
        })
    }
}

/// Подробные сообщения о кодировках — только в CLI (в GUI журнал короткий).
fn console_verbose() -> bool {
    std::env::var("SR_QUIET").is_err()
}

fn label(kind: &str) -> &'static str {
    match kind {
        "html" => "HTML",
        "code" => "CSS/JS/XML",
        "img" => "IMG",
        _ => "OTHER",
    }
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
}

impl Index {
    fn of(root: &Path) -> Self {
        let cache_rx = Regex::new(
            r"(?i)^bitrix/cache/(css|js)/s1/[^/]+/[^/]+/([a-z_]+)_[0-9a-f]+_v1",
        )
        .unwrap();
        let resize_rx =
            Regex::new(r"(?i)^upload/resize_cache/iblock/([0-9a-z]+)/[^/]+/(.+)$").unwrap();
        let mut index = Self {
            paths: HashMap::new(),
            by_name: HashMap::new(),
            by_kind: HashMap::new(),
            resized: HashMap::new(),
            cache_rx: cache_rx.clone(),
            resize_rx,
        };
        for path in walk_files(root) {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
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
        let key = rel.trim_start_matches('/').to_lowercase();
        if let Some(found) = self.paths.get(&key) {
            return Some(found.clone());
        }
        if let Some(caps) = self.cache_rx.captures(&key) {
            let slot = (caps[1].to_lowercase(), caps[2].to_lowercase());
            if let Some(found) = self.by_kind.get(&slot) {
                return Some(found.clone());
            }
        }
        if let Some(caps) = self.resize_rx.captures(&key) {
            let orig = format!("upload/iblock/{}/{}", &caps[1], &caps[2]).to_lowercase();
            if let Some(found) = self.paths.get(&orig) {
                return Some(found.clone());
            }
        }
        // Обратная эвристика: страница ссылается на оригинал, а в архиве сохранена
        // только уменьшенная копия (типичная ситуация у Bitrix-каталогов).
        if let Some(rest) = key.strip_prefix("upload/iblock/") {
            if let Some((folder, name)) = rest.split_once('/') {
                if let Some(found) = self.resized.get(&(folder.to_string(), name.to_string())) {
                    return Some(found.clone());
                }
            }
        }
        let base = key.rsplit('/').next().unwrap_or(&key).to_string();
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
        if let Ok(n) = digits.parse::<usize>() {
            if n > 0 {
                return Some(n);
            }
        }
        from += pos + "numpages".len();
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

const REPORT_TMPL: &str = r#"<!DOCTYPE html><html lang="ru"><head><meta charset="utf-8">
<title>Отчёт: {{host}}</title>
<style>body{font:15px/1.5 "Segoe UI",Arial;background:#f7f8fa;color:#16181d;margin:0}
.w{max-width:860px;margin:0 auto;padding:36px 22px}h1{font-size:26px;margin:0 0 4px}
.s{color:#6b7280;margin-bottom:24px}.g{display:grid;grid-template-columns:repeat(auto-fit,minmax(160px,1fr));gap:12px}
.c{background:#fff;border:1px solid #e5e7eb;border-radius:10px;padding:14px 16px}
.k{font-size:12px;text-transform:uppercase;letter-spacing:.07em;color:#6b7280}
.v{font-size:24px;font-weight:650}table{width:100%;border-collapse:collapse;background:#fff;border:1px solid #e5e7eb;border-radius:10px;margin-top:14px}
th,td{padding:9px 12px;border-bottom:1px solid #eee;text-align:left}th{background:#fafbfc}
.n{text-align:right;font-variant-numeric:tabular-nums}h2{font-size:18px;margin:28px 0 8px}</style></head><body><div class="w">
<h1>Копия сайта: {{host}}</h1><div class="s">Отчёт о покрытии архива</div>
<div class="g"><div class="c"><div class="k">Скачано</div><div class="v">{{ok}} / {{total}}</div></div>
<div class="c"><div class="k">Покрытие</div><div class="v">{{pct}}%</div></div>
<div class="c"><div class="k">Объём</div><div class="v">{{mb}} МБ</div></div>
<div class="c"><div class="k">Страниц</div><div class="v">{{pages}}</div></div></div>
<h2>По типам файлов</h2><table><tr><th>Тип</th><th class="n">Скачано</th><th class="n">Всего</th><th class="n">Доля</th></tr>{{rows}}</table>
<h2>Целостность</h2><table>
<tr><td>Внутренних ссылок</td><td class="n">{{refs}}</td></tr>
<tr><td>Битых ссылок <span style="color:#6b7280">(нет в копии)</span></td><td class="n">{{broken}}</td></tr>
{{encodings}}
<tr><td>Отказов 404 при загрузке</td><td class="n">{{missing}}</td></tr>
<tr><td>Ошибок загрузки</td><td class="n">{{errors}}</td></tr></table>
<p style="color:#6b7280;margin-top:24px">Статическая копия: корзина, вход и AJAX исходного сайта не работают.
Открывайте папку <b>site</b> любым локальным веб-сервером.</p>
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
        assert_eq!(parse_pages(r#"[["numpages"],["912"]]"#), Some(912));
        assert_eq!(parse_pages(r#"{"pages": 3}"#), None);
        assert_eq!(parse_pages("12"), Some(12));
        assert_eq!(parse_pages(""), None);
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

    #[test]
    fn cp1251_page_is_converted_to_utf8() {
        let root = workdir("charset");
        let page = "<html><head><meta charset=\"windows-1251\"></head><body><h1>Привет</h1></body></html>";
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
        assert!(meta_at < 1024, "meta должна попасть в первые 1024 байта: {meta_at}");
        assert!(out.contains("charset=utf-8"));

        let early = "<html><head><meta charset=\"utf-8\"></head><body>x</body></html>";
        assert_eq!(crate::encoding::normalize_utf8(early), early, "корректный документ не меняем");
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
        put(&root, "index.html", b"<html><body><a href='/page2/'>x</a></body></html>");
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
        assert!(html.contains("href='../css/s.css'") || html.contains("href=\"../css/s.css\""), "css-ссылка: {html}");
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
