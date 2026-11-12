//! Ядро: инвентаризация архива, параллельная загрузка с AIMD, сборка оффлайн-копии,
//! проверка ссылок и отчёт.

use crate::paths;
use crate::sanitize::sanitize_site;

use rayon::prelude::*;
use regex::Regex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub const UA: &str =
    "SiteRestorer/2.0 (offline mirror tool; respectful crawler; contact: local user)";
const MIN_THREADS: usize = 2;
const MAX_THREADS: usize = 24;
const PAGE_LIMIT: usize = 20_000;
/// Основной адрес архива: сначала шифрованный канал, открытый — запасной.
/// Часть сетей блокирует порт 80, часть — только 443 (ОШ-2 в docs/DEFECTS.md).
const ARCHIVE_PRIMARY: &str = "https://web.archive.org";
const ARCHIVE_FALLBACK: &str = "http://web.archive.org";
/// Сколько попыток даётся на обращение к индексу архива.
const ARCHIVE_TRIES: u32 = 5;
/// Пределы ожидания (ПР-5, Д-18): файл, запрос индекса; третий предел — общий 180 с.
const TIMEOUT_FILE: u64 = 60;
const TIMEOUT_INDEX: u64 = 120;
/// Общий предел запроса (ПР-5): внешняя страховка поверх двух предыдущих.
const TIMEOUT_TOTAL: u64 = 180;
// Проверка согласованности: индекс ждёт заголовки TIMEOUT_FILE и тело TIMEOUT_INDEX,
// вместе это ровно TIMEOUT_TOTAL.
const _: () = assert!(TIMEOUT_FILE + TIMEOUT_INDEX == TIMEOUT_TOTAL);
/// Размер выборки быстрого запроса индекса (ФТ-149).
/// Архив отдаёт примерно столько строк, сколько попросили, но шапка JSON и
/// неразобранные строки съедают несколько: на nginx.org при реальном усечении
/// разбор дал 49 990 строк вместо 50 000. Поэтому усечение определяется не
/// точным равенством, а попаданием в полосу допуска (проверено 12.11.2026).
const QUICK_LIMIT: usize = 50_000;
/// Сколько первых байт тела проверяется как «голова» файла (Д-50).
/// Все проверки пригодности (пустая страница, страница ошибки, двоичность)
/// смотрят только в начало, поэтому при потоковой записи тело в память
/// целиком не читается. 4096 выбрано по замеру: самая длинная проверяемая
/// сигнатура укладывается в 2000 байт, прежний буфер 256 давал ложные отказы.
const HEAD_BYTES: usize = 4096;

/// Допуск на шапку JSON и потери разбора при проверке усечения.
const QUICK_SLACK: usize = 32;
/// Предел страниц обхода по умолчанию (ФТ-149), задаётся `--index-max-pages`.
const DEFAULT_INDEX_MAX_PAGES: usize = 200;
/// Сколько более старых снимков пробовать, если свежий непригоден (ФТ-127).
const DEFAULT_FALLBACK_DEPTH: usize = 5;
/// Верхний предел глубины добора: больше — это уже обход архива, а не добор.
const MAX_FALLBACK_DEPTH: usize = 20;
/// Через сколько дней снимок считается устаревшим для повторного прогона (ФТ-130).
const DEFAULT_REFRESH_DAYS: u32 = 30;
/// Предел добора по ссылкам: сколько отсутствующих целей догружать за прогон (ФТ-128).
const DEFAULT_RESCUE_LIMIT: usize = 500;
/// Порог «пустой страницы»: короче — считаем заглушкой и берём более старый снимок (ФТ-127).
const DEFAULT_STUB_BYTES: usize = 256;
/// Бюджет времени на добор по ссылкам: без него прогон висит на тысячах запросов.
const DEFAULT_RESCUE_BUDGET: u64 = 30;
/// Через сколько дней повторять адрес, у которого не было пригодного снимка.
const DEFAULT_RETRY_MISSING_DAYS: u32 = 7;
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
    /// Длина записи в архиве (сжатые байты): по ней оценивается потребность в месте.
    pub len: u64,
    /// Отпечаток содержимого из индекса (ФТ-145). Пустая строка — архив не отдал.
    pub digest: String,
    /// Путь канонического файла, если этот получен ссылкой (ФТ-146).
    pub same_as: String,
}

/// Как сопоставлять адрес с индексом (ФТ-149).
///
/// `Domain` — домен и поддомены (по умолчанию). На крупных доменах CDX отвечает 504,
/// поэтому при отказе переключаемся на `Prefix` и повторяем запрос.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchType {
    Domain,
    Prefix,
}

/// Режим выбора снимка (ФТ-126).
///
/// `Newest` — цель «сейчас»: архив отдаёт самый свежий снимок адреса.
/// `Oldest` — отпечаток из индекса, то есть самый ранний снимок: поведение 3.4.0,
/// нужно для воспроизводимости прежних копий.
/// `At` — снимок, ближайший к указанной дате.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnapshotMode {
    Newest,
    Oldest,
    At(String),
}

impl SnapshotMode {
    /// Разбор значения командной строки. Дата принимается как 4, 6, 8 или 14 цифр.
    pub fn parse(text: &str) -> Option<Self> {
        let value = text.trim().to_ascii_lowercase();
        match value.as_str() {
            "newest" | "latest" | "fresh" => return Some(Self::Newest),
            "oldest" | "first" => return Some(Self::Oldest),
            _ => {}
        }
        let digits: String = value.chars().filter(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() || digits.len() > 14 {
            return None;
        }
        let mut stamp = digits;
        while stamp.len() < 14 {
            stamp.push('0');
        }
        Some(Self::At(stamp))
    }

    /// Метка для журнала сборки.
    fn tag(&self) -> String {
        match self {
            Self::Newest => "newest".to_string(),
            Self::Oldest => "oldest".to_string(),
            Self::At(stamp) => format!("at:{stamp}"),
        }
    }

    /// Целевая дата воспроизведения для адреса с отпечатком `ts`.
    fn target(&self, ts: &str) -> String {
        match self {
            Self::Newest => stamp_now(),
            Self::Oldest => ts.to_string(),
            Self::At(stamp) => stamp.clone(),
        }
    }
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

/// Чем закончилась попытка получить снимок по одному отпечатку.
enum Attempt {
    /// Снимок пригоден и записан.
    Saved {
        size: usize,
        /// Фактическое время снимка из ответа (`Memento-Datetime`).
        memento: Option<String>,
    },
    /// Снимка по этому отпечатку нет (архив ответил 404).
    Absent,
    /// Снимок есть, но содержимое непригодно: пусто, заглушка, чужой адрес.
    Unusable,
    /// Сбой сети или отказ архива после повторов.
    Failed,
}

/// Ответ архива.
/// Заголовки ответа без тела: общая часть всех видов запроса.
#[derive(Clone, Debug, Default)]
pub struct Head {
    pub status: u16,
    pub retry_after: Option<f64>,
    pub content_type: Option<String>,
    /// Фактическое время отданного снимка (`Memento-Datetime`, ФТ-129).
    pub memento: Option<String>,
}

/// Результат потоковой загрузки (Д-50): файл уже на диске в `.part`,
/// в памяти остались только первые байты для проверок.
struct Saved {
    head: Head,
    /// Сколько байт записано всего.
    len: u64,
    /// Первые `HEAD_BYTES` байт тела.
    head_bytes: Vec<u8>,
    /// Временный файл, который станет целевым после проверок.
    tmp: PathBuf,
}

struct Fetched {
    status: u16,
    body: Vec<u8>,
    retry_after: Option<f64>,
}

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
    /// Сколько файлов получено добором к более старым снимкам (ФТ-127).
    pub fallbacks: usize,
    /// Сколько адресов отброшено из-за совпадения путей копии (ФТ-131).
    pub collisions: usize,
    /// Сколько целей добрано по ссылкам (ФТ-128).
    pub rescued: usize,
    /// Файлов получено ссылкой на уже скачанный (ФТ-146).
    pub shared: usize,
    /// Абсолютных адресов своего домена, оставленных как есть (ФТ-147).
    pub left: usize,
    /// Адресов в индексе до пользовательского --limit (ФТ-155).
    pub index_total: usize,
    /// Сколько страниц индекса обойдено (ФТ-155).
    pub index_pages: usize,
    /// Индекс остановлен по пределу или размеру выборки (ФТ-155).
    pub index_truncated: bool,
    /// Индекс не получен: отказы архива после повторов (ФТ-150, ФТ-155).
    pub index_failed: bool,
    /// Адресов добрано на ходу при загрузке (ФТ-152, ФТ-155).
    pub added_live: usize,
    /// Адресов добрано по ссылкам при сборке (ФТ-153, ФТ-155).
    pub added_build: usize,
    /// Самый старый и самый новый снимок в копии: `ГГГГ-ММ-ДД` (ФТ-129).
    pub oldest: String,
    pub newest: String,
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

// ------------------------------------------------------------------- queue
/// Пополняемая очередь загрузки (Д-55).
///
/// Отличие от прежнего списка с `AtomicUsize`: адрес можно добавить во время
/// работы, а воркеры ждут на условной переменной вместо выхода по «индекс за
/// концом». Без ожидания адрес, пришедший в момент выхода, терялся.
struct Queue {
    items: Mutex<VecDeque<Item>>,
    cv: Condvar,
    closed: AtomicBool,
    /// Пути, уже стоявшие в очереди: повторный адрес не добавляется.
    seen: Mutex<HashSet<String>>,
    /// Сколько адресов прошло через очередь: знаменатель прогресса.
    planned: AtomicUsize,
}

impl Queue {
    /// `base` — сколько адресов уже было на диске к началу загрузки: они входят
    /// в счётчик `done`, поэтому и в знаменатель прогресса должны входить, иначе
    /// на докачке «done >= planned» истинно сразу и добор не успевает работать.
    fn new(items: Vec<Item>, base: usize) -> Self {
        let mut seen = HashSet::with_capacity(items.len());
        for item in &items {
            seen.insert(item.path.clone());
        }
        let planned = AtomicUsize::new(base + items.len());
        Self {
            items: Mutex::new(items.into_iter().collect()),
            cv: Condvar::new(),
            closed: AtomicBool::new(false),
            seen: Mutex::new(seen),
            planned,
        }
    }

    /// Добавить адрес, если его пути ещё не было. Возвращает true при добавлении.
    fn push(&self, item: Item) -> bool {
        {
            let mut seen = self.seen.lock().unwrap();
            if !seen.insert(item.path.clone()) {
                return false;
            }
        }
        self.planned.fetch_add(1, Ordering::Relaxed);
        self.items.lock().unwrap().push_back(item);
        self.cv.notify_one();
        true
    }

    /// Взять адрес; ждать, пока очередь опустеет, но не после закрытия.
    fn pop(&self) -> Option<Item> {
        let mut items = self.items.lock().unwrap();
        loop {
            if let Some(item) = items.pop_front() {
                return Some(item);
            }
            if self.closed.load(Ordering::Relaxed) {
                return None;
            }
            items = self.cv.wait(items).unwrap();
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        self.cv.notify_all();
    }

    fn planned(&self) -> usize {
        self.planned.load(Ordering::Relaxed)
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
    /// Адреса без пригодного снимка (ФТ-127).
    no_snapshot: AtomicUsize,
    /// Сколько файлов получено добором к более старым снимкам (ФТ-127).
    fallbacks: AtomicUsize,
    /// Сколько адресов отброшено как совпавшие по пути копии (ФТ-131).
    collisions: AtomicUsize,
    /// Сколько адресов добрано на ходу во время загрузки (ФТ-152).
    added_live: AtomicUsize,
    /// Сколько файлов получено ссылкой на уже скачанный (ФТ-146).
    shared: AtomicUsize,
    /// Сколько абсолютных адресов своего домена оставлено как есть: цели нет
    /// в копии либо это не файл (ФТ-147).
    left: AtomicUsize,
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
    /// Отдельный агент для индекса: предел ожидания 120 с вместо 60 (Д-18).
    agent_index: ureq::Agent,
    /// Текущий режим сопоставления адреса с индексом (ФТ-149).
    match_type: Mutex<MatchType>,
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
    /// Обработчик прогресса ставится один раз и дальше не меняется.
    /// Раньше здесь был Option, а установка пересоздавала весь Shared и вручную
    /// переносила два десятка полей — новое поле легко было забыть и получить
    /// молчаливый ноль (2.3 архитектурного разбора).
    cb: std::sync::OnceLock<Cb>,
    /// Глобальная пауза после серии 429: её соблюдают все воркеры, а не только контроллер.
    pause_until: Mutex<Instant>,
    /// Пользователь поставил прогон на паузу (Д-20): новые запросы ждут.
    paused: Arc<AtomicBool>,
    /// Карта «отпечаток содержимого → путь канонического файла» (ФТ-145).
    /// Живёт в пределах прогона: по ней адрес с уже скачанным содержимым
    /// получает жёсткую ссылку вместо второго запроса.
    dedup: Mutex<HashMap<String, (String, String)>>,
    /// Обратная сторона: путь копии → канонический путь (поле `same_as`).
    shared_paths: Mutex<HashMap<String, String>>,
    /// Режим выбора снимка (ФТ-126): нужен воркерам.
    snapshot: Mutex<SnapshotMode>,
    /// Порог «пустой страницы» (ФТ-127).
    stub_bytes: AtomicUsize,
    /// Глубина добора к более старым снимкам (ФТ-127).
    fallback_depth: AtomicUsize,
    /// Файлы, которые надо скачать заново, несмотря на наличие на диске (ФТ-130).
    refresh: Mutex<HashSet<String>>,
    /// Адреса без пригодного снимка: url -> когда проверяли (ФТ-135).
    no_snapshot: Mutex<HashMap<String, u64>>,
    /// Дата снимка каждого файла зеркала: путь относительно зеркала -> отпечаток.
    snapshots: Mutex<HashMap<String, String>>,
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
    /// Запомнить адрес, у которого не было пригодного снимка (ФТ-135):
    /// повторять его каждый прогон — это десятки лишних запросов к архиву.
    fn note_no_snapshot(&self, item: &Item) {
        if let Ok(mut map) = self.no_snapshot.lock() {
            map.insert(item.url.clone(), now_secs());
        }
    }

    fn note_foreign(&self) {
        self.foreign.fetch_add(1, Ordering::Relaxed);
    }

    fn log(&self, msg: impl Into<String>) {
        if let Some(cb) = self.cb.get() {
            cb(&Progress {
                stage: "log",
                text: msg.into(),
                ..Default::default()
            });
        }
    }

    /// Заголовки ответа: всё, что нужно вызывающему до чтения тела.
    ///
    /// Вынесено отдельно, чтобы `fetch`, `fetch_stream` и потоковая запись (Д-50)
    /// собирались одной шапкой: раньше она была скопирована дважды, и каждая
    /// новая доработка добавляла третью копию.
    fn open(&self, index: bool, url: &str) -> Result<(Head, ureq::Body), String> {
        self.wait_if_paused();
        self.rate_wait();
        let agent = if index {
            &self.agent_index
        } else {
            &self.agent
        };
        let resp = agent
            .get(url)
            .header("User-Agent", UA)
            .header("Accept", "*/*")
            .call()
            .map_err(|e| e.to_string())?;
        let head = Head {
            status: resp.status().as_u16(),
            retry_after: resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<f64>().ok()),
            content_type: resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .map(|v| v.to_string()),
            memento: resp
                .headers()
                .get("memento-datetime")
                .and_then(|v| v.to_str().ok())
                .and_then(stamp_from_http_date),
        };
        Ok((head, resp.into_body()))
    }

    /// Прочитать тело целиком. Между блоками проверяется отмена (Д-23):
    /// остановка не ждёт таймаута запроса, соединение закрывается сразу.
    /// Побочно снимается потолок 10 МиБ, который был у `read_to_vec` в ureq 3.
    fn read_all(&self, body: ureq::Body) -> Result<Vec<u8>, String> {
        let mut out: Vec<u8> = Vec::new();
        let mut reader = body.into_reader();
        let mut chunk = [0u8; 64 * 1024];
        loop {
            if self.cancel.load(Ordering::Relaxed) {
                return Err("cancelled".to_string());
            }
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&chunk[..n]),
                Err(err) => return Err(err.to_string()),
            }
        }
        Ok(out)
    }

    /// То же чтение, но с отчётом о прогрессе: `on_progress(получено байт, число строк)`.
    /// Нужно для индекса — ответ в десятки мегабайт идёт долго, и без счётчиков
    /// полоса «замирает».
    fn read_progress(
        &self,
        body: ureq::Body,
        on_progress: &mut impl FnMut(u64, usize),
    ) -> Result<Vec<u8>, String> {
        let mut out: Vec<u8> = Vec::new();
        let mut lines = 0usize;
        let mut last = Instant::now();
        let mut reader = body.into_reader();
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
            out.extend_from_slice(part);
            lines += part.iter().filter(|b| **b == b'\n').count();
            if last.elapsed() >= Duration::from_millis(200) {
                on_progress(out.len() as u64, lines);
                last = Instant::now();
            }
        }
        on_progress(out.len() as u64, lines);
        Ok(out)
    }

    fn fetch(&self, url: &str) -> Result<Fetched, String> {
        let (head, body) = self.open(false, url)?;
        Ok(Fetched {
            status: head.status,
            body: self.read_all(body)?,
            retry_after: head.retry_after,
        })
    }

    /// То же, что `fetch`, но с отчётом о прогрессе чтения (индекс).
    fn fetch_stream(
        &self,
        url: &str,
        mut on_progress: impl FnMut(u64, usize),
    ) -> Result<Fetched, String> {
        let (head, body) = self.open(true, url)?;
        Ok(Fetched {
            status: head.status,
            body: self.read_progress(body, &mut on_progress)?,
            retry_after: head.retry_after,
        })
    }

    /// Сколько секунд осталось до конца общей паузы.
    fn pause_left(&self) -> u64 {
        let until = *self.pause_until.lock().unwrap();
        until.saturating_duration_since(Instant::now()).as_secs()
    }

    /// Поставить глобальную паузу: воркеры увидят её перед следующим запросом.
    fn pause(&self, seconds: f64) {
        let until = Instant::now() + Duration::from_secs_f64(seconds.max(0.0));
        *self.pause_until.lock().unwrap() = until;
    }

    /// Ждёт, пока пользователь не снимет паузу (Д-20). Остановка прерывает ожидание.
    fn wait_for_pause(&self) {
        while self.paused.load(Ordering::Relaxed) {
            if self.cancel.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
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

    /// Получает снимок адреса: сначала самый свежий пригодный, затем — добор к более
    /// старым (ФТ-126, ФТ-127). Старые снимки запрашиваются только при непригодности
    /// свежего; сбой сети добором не заменяется.
    /// Взять файл ссылкой на уже скачанный (ФТ-146).
    ///
    /// Возвращает размер, если ссылка поставлена. Жёсткая ссылка работает только
    /// внутри одного тома (проверено: другой том даёт `CrossesDevices`, предел
    /// NTFS — 1024 ссылки), поэтому при отказе файл копируется: содержимое важнее
    /// экономии места.
    fn link_shared(&self, item: &Item, canon: &str) -> Option<usize> {
        if canon == item.path {
            return None;
        }
        let from = self.mirror.join(canon);
        let to = self.mirror.join(&item.path);
        fs::metadata(&from).ok().filter(|m| m.len() > 0)?;
        if let Some(parent) = to.parent() {
            let _ = fs::create_dir_all(parent);
        }
        // Занятое имя мешает и ссылке, и переименованию: убираем старый файл.
        let _ = fs::remove_file(&to);
        if fs::hard_link(&from, &to).is_err() && fs::copy(&from, &to).is_err() {
            return None;
        }
        let size = fs::metadata(&to).ok()?.len() as usize;
        if let Ok(mut map) = self.shared_paths.lock() {
            map.insert(item.path.clone(), canon.to_string());
        }
        if let Ok(ct) = self.content_types.lock()
            && let Some(value) = ct.get(canon).cloned()
        {
            drop(ct);
            self.content_types
                .lock()
                .unwrap()
                .insert(item.path.clone(), value);
        }
        Some(size)
    }

    fn fetch_one(&self, item: &Item) {
        if self.cancel.load(Ordering::Relaxed) {
            return;
        }
        let full = self.mirror.join(&item.path);
        if let Ok(meta) = fs::metadata(&full)
            && meta.len() > 0
        {
            let stale = self
                .refresh
                .lock()
                .map(|set| set.contains(&item.path))
                .unwrap_or(false);
            if !stale {
                self.counters.done.fetch_add(1, Ordering::Relaxed);
                self.counters.ok.fetch_add(1, Ordering::Relaxed);
                self.types.lock().unwrap().slot(item.kind)[0] += 1;
                return;
            }
            // Снимок устарел: файл удаляем, иначе цепочка сочтёт его готовым.
            let _ = fs::remove_file(long_path(&full));
        }

        // Уже скачанное содержимое: второй запрос не нужен (ФТ-145, ФТ-146).
        // Отпечаток приходит из индекса, поэтому проверка дешёвая.
        // Ссылка ставится только при совпадении отпечатка И снимка: отпечаток
        // относится к записи индекса, а скачивается конкретный снимок (ФТ-145).
        if !item.digest.is_empty()
            && let Some((canon, canon_ts)) = self
                .dedup
                .lock()
                .ok()
                .and_then(|m| m.get(&item.digest).cloned())
            && canon_ts == item.ts
            && let Some(size) = self.link_shared(item, &canon)
        {
            self.counters.done.fetch_add(1, Ordering::Relaxed);
            self.counters.ok.fetch_add(1, Ordering::Relaxed);
            self.counters.shared.fetch_add(1, Ordering::Relaxed);
            self.types.lock().unwrap().slot(item.kind)[0] += 1;
            if let Ok(mut map) = self.snapshots.lock() {
                map.insert(item.path.clone(), item.ts.clone());
            }
            if console_verbose() {
                self.log(crate::tr::pick(
                    format!("shared:{} — {}", canon, item.path),
                    format!("shared:{} — {}", canon, item.path),
                ));
            }
            let _ = size;
            return;
        }

        let mode = self
            .snapshot
            .lock()
            .map(|m| m.clone())
            .unwrap_or(SnapshotMode::Newest);
        let stub = self.stub_bytes.load(Ordering::Relaxed);
        let depth = self.fallback_depth.load(Ordering::Relaxed);

        let target = mode.target(&item.ts);
        let mut result = self.attempt(item, &target, stub);
        let mut used_stamp = target.clone();
        let mut fell_back = false;

        // Добор запускается только при отсутствии или непригодности (ФТ-127).
        if depth > 0 && matches!(result, Attempt::Absent | Attempt::Unusable) {
            for stamp in self.recent_stamps(item, depth) {
                if self.cancel.load(Ordering::Relaxed) {
                    return;
                }
                if stamp == target {
                    continue;
                }
                if let Attempt::Saved { .. } = result {
                    break;
                }
                result = self.attempt(item, &stamp, stub);
                used_stamp = stamp;
                fell_back = true;
                if matches!(result, Attempt::Saved { .. } | Attempt::Failed) {
                    break;
                }
            }
        }

        self.counters.done.fetch_add(1, Ordering::Relaxed);
        match result {
            Attempt::Saved { size, memento } => {
                self.counters.ok.fetch_add(1, Ordering::Relaxed);
                self.counters
                    .bytes
                    .fetch_add(size as u64, Ordering::Relaxed);
                self.counters
                    .win_bytes
                    .fetch_add(size as u64, Ordering::Relaxed);
                self.counters.win_files.fetch_add(1, Ordering::Relaxed);
                self.types.lock().unwrap().slot(item.kind)[0] += 1;
                if fell_back {
                    self.counters.fallbacks.fetch_add(1, Ordering::Relaxed);
                    if console_verbose() {
                        self.log(crate::tr::pick(
                            format!(
                                "snapshot:fallback:{} — {}",
                                memento.as_deref().unwrap_or(&used_stamp),
                                item.path
                            ),
                            format!(
                                "snapshot:fallback:{} — {}",
                                memento.as_deref().unwrap_or(&used_stamp),
                                item.path
                            ),
                        ));
                    }
                }
                let stamp = memento.unwrap_or(used_stamp);
                if let Ok(mut map) = self.snapshots.lock() {
                    map.insert(item.path.clone(), stamp);
                }
                // Отпечаток запоминаем только для реально скачанного файла:
                // по нему следующие адреса возьмут ссылку.
                if !item.digest.is_empty()
                    && let Ok(mut map) = self.dedup.lock()
                {
                    map.entry(item.digest.clone())
                        .or_insert_with(|| (item.path.clone(), item.ts.clone()));
                }
            }
            Attempt::Absent => {
                self.counters.missing.fetch_add(1, Ordering::Relaxed);
                self.counters.no_snapshot.fetch_add(1, Ordering::Relaxed);
                self.note_no_snapshot(item);
                self.log(crate::tr::pick(
                    format!("snapshot:none — {}", item.path),
                    format!("snapshot:none — {}", item.path),
                ));
            }
            Attempt::Unusable => {
                self.counters.missing.fetch_add(1, Ordering::Relaxed);
                self.counters.no_snapshot.fetch_add(1, Ordering::Relaxed);
                self.note_no_snapshot(item);
                self.log(crate::tr::pick(
                    format!("snapshot:unusable — {}", item.path),
                    format!("snapshot:unusable — {}", item.path),
                ));
            }
            Attempt::Failed => {
                self.counters.errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Одна попытка: `/web/<отпечаток>id_/<адрес>`.
    fn attempt(&self, item: &Item, stamp: &str, stub: usize) -> Attempt {
        let url = format!("{}/web/{stamp}id_/{}", self.archive_base(), item.url);
        let mut transport_failed = false;
        for attempt in 0..2u32 {
            if self.cancel.load(Ordering::Relaxed) {
                return Attempt::Failed;
            }
            self.wait_for_pause();
            // Один запрос на файл: заголовки решают, что делать с телом, и оно же
            // пишется на диск. Раньше здесь сначала шёл `fetch` (тело в память),
            // а затем `save_to` качал тот же файл вторым запросом — трафик ×2.
            let (head, body) = match self.open(false, &url) {
                Ok(pair) => pair,
                Err(err) => {
                    transport_failed = true;
                    self.log(crate::tr::pick(
                        format!("сеть: {err}"),
                        format!("network: {err}"),
                    ));
                    // Закрытый порт схемы: пробуем другую схему (ОШ-2).
                    self.switch_archive("сеть недоступна");
                    self.sleep(0.6 * (attempt + 1) as f64);
                    continue;
                }
            };
            let status = head.status;
            if status == 429 {
                self.counters.win_429.fetch_add(1, Ordering::Relaxed);
                let wait = head.retry_after.unwrap_or(20.0 * (attempt + 1) as f64);
                self.log(crate::tr::pick(
                    format!("429: пауза {wait:.0} с"),
                    format!("429: pause {wait:.0}s"),
                ));
                self.sleep(wait);
                continue;
            }
            if status == 404 {
                return Attempt::Absent;
            }
            if status == 200 {
                // 200 — тело того же ответа пишется в файл потоком.
                return match self.save_body(head, body, &self.mirror.join(&item.path)) {
                    Ok(saved) => match self.commit(item, &saved, stub) {
                        Some(done) => done,
                        None => Attempt::Unusable,
                    },
                    Err(err) if err == "cancelled" => Attempt::Failed,
                    Err(err) => {
                        self.log(crate::tr::pick(
                            format!("запись: {err}"),
                            format!("write: {err}"),
                        ));
                        Attempt::Failed
                    }
                };
            }
            // 3xx без пригодного содержимого и прочие коды: содержимого нет.
            if (300..400).contains(&status) {
                return Attempt::Unusable;
            }
            transport_failed = true;
            self.sleep(0.8 * (attempt + 1) as f64);
        }
        if transport_failed {
            Attempt::Failed
        } else {
            Attempt::Unusable
        }
    }

    /// Скачать тело сразу в файл, не собирая его в памяти (Д-50, ПР-11).
    ///
    /// Тело пишется в `.part` рядом с целевым файлом: недокачанный файл не должен
    /// выглядеть готовым. `head_bytes` — первые `HEAD_BYTES` байт, по ним потом
    /// проверяется пригодность; счётчики байтов здесь НЕ трогаются, иначе AIMD
    /// увидел бы двойную скорость (байты начисляются один раз по размеру файла).
    /// Записать уже открытое тело в файл (тот же `save_to`, но без второго запроса).
    fn save_body(&self, head: Head, body: ureq::Body, dest: &Path) -> Result<Saved, String> {
        if let Some(parent) = dest.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let tmp = tmp_path(dest);
        let mut file = fs::File::create(&tmp).map_err(|e| e.to_string())?;
        let mut reader = body.into_reader();
        let mut chunk = [0u8; 64 * 1024];
        let mut len: u64 = 0;
        let mut head_bytes: Vec<u8> = Vec::new();
        let cleanup = |tmp: &Path| {
            let _ = fs::remove_file(tmp);
        };
        loop {
            if self.cancel.load(Ordering::Relaxed) {
                drop(file);
                cleanup(&tmp);
                return Err("cancelled".to_string());
            }
            let read = match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => n,
                Err(err) => {
                    drop(file);
                    cleanup(&tmp);
                    return Err(err.to_string());
                }
            };
            let part = &chunk[..read];
            if head_bytes.len() < HEAD_BYTES {
                let take = (HEAD_BYTES - head_bytes.len()).min(read);
                head_bytes.extend_from_slice(&part[..take]);
            }
            if let Err(err) = file.write_all(part) {
                drop(file);
                cleanup(&tmp);
                return Err(err.to_string());
            }
            len += read as u64;
        }
        drop(file);
        Ok(Saved {
            head,
            len,
            head_bytes,
            tmp,
        })
    }

    /// Проверяет пригодность скачанного и ставит файл на место.
    /// `None` — содержимое непригодно, временный файл удаляется.
    fn commit(&self, item: &Item, saved: &Saved, stub: usize) -> Option<Attempt> {
        let head = &saved.head_bytes;
        let html = item.kind == "html";
        // «Пустая страница» проверяется по полному размеру, остальное — по голове.
        let small_page = html && (saved.len < stub.max(20) as u64 || !looks_like_markup(head));
        let usable = saved.len > 0 && !is_error_page(head) && !small_page;
        if !usable {
            let _ = fs::remove_file(&saved.tmp);
            return None;
        }
        let full = self.mirror.join(&item.path);
        if fs::rename(&saved.tmp, &full).is_err() {
            // Windows: rename НЕ перезаписывает существующий файл, поэтому сначала
            // убираем старый (docs/PLATFORMS.md, «атомарная запись»). Копирование
            // оставлено последней страховкой для чужих файловых систем.
            let _ = fs::remove_file(&full);
            if fs::rename(&saved.tmp, &full).is_err() {
                let _ = fs::copy(&saved.tmp, &full);
                let _ = fs::remove_file(&saved.tmp);
            }
        }
        // Файл мог не пережить переименование: успех считаем только по факту.
        let size = fs::metadata(&full).ok().filter(|m| m.len() > 0)?.len() as usize;
        if html && crate::encoding::looks_binary(head) {
            self.note_foreign();
            if console_verbose() && self.foreign_logged.fetch_add(1, Ordering::Relaxed) < 3 {
                self.log(crate::tr::pick(
                    format!("вместо страницы — двоичный файл: {}", item.path),
                    format!("binary content behind a page path: {}", item.path),
                ));
            }
        }
        if let Some(ct) = &saved.head.content_type {
            self.content_types
                .lock()
                .unwrap()
                .insert(item.path.clone(), ct.clone());
        }
        Some(Attempt::Saved {
            size,
            memento: saved.head.memento.clone(),
        })
    }

    /// Последние снимки адреса по индексу, от свежего к старому (для добора).
    fn recent_stamps(&self, item: &Item, depth: usize) -> Vec<String> {
        let query = format!(
            "{}/cdx/search/cdx?url={}&output=json&fl=timestamp&filter=statuscode:200&limit=-{}",
            self.archive_base(),
            urlencode(&item.url),
            depth
        );
        let Ok(fetched) = self.fetch(&query) else {
            return Vec::new();
        };
        if fetched.status != 200 {
            return Vec::new();
        }
        let mut stamps: Vec<String> = parse_rows(&fetched.body)
            .into_iter()
            .filter_map(|row| row.first().cloned())
            .collect();
        stamps.sort_by(|a, b| b.cmp(a));
        stamps
    }
}

/// Свернуть одну строку CDX в карту «адрес → самый поздний снимок» (2.4).
///
/// Строка: `[отпечаток, исходный адрес, тип, код, длина]`. Если адрес уже есть,
/// остаётся снимок с большим отпечатком. `from_window = false` означает добор из
/// полной истории: тогда страницы не трогаются — они уже взяты из окна (ФТ-128).
fn fold_row(
    best: &mut HashMap<String, (String, String, String, u64, String)>,
    host: &str,
    row: &[String],
    from_window: bool,
) {
    if row.len() < 3 {
        return;
    }
    let orig = row[1].clone();
    let url = paths::normalize_url(&orig, host);
    if !from_window && paths::file_type(&url) == "html" && best.contains_key(&url) {
        return;
    }
    let ts = row[0].clone();
    let mime = row[2].clone();
    let len = row.get(4).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    // Шестое поле запроса — отпечаток содержимого (ФТ-145); последняя строка
    // может прийти короче, тогда отпечатка просто нет.
    let digest = row.get(5).cloned().unwrap_or_default();
    match best.get(&url) {
        Some((old_ts, _, _, _, _)) if *old_ts >= ts => {}
        _ => {
            best.insert(url, (ts, orig, mime, len, digest));
        }
    }
}

/// Временный файл прошлого прогона: `.part` или `.jsonl.part`.
fn is_temp_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(".part") || n.ends_with(".jsonl.part"))
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
    /// Режим выбора снимка (ФТ-126).
    pub snapshot: SnapshotMode,
    /// Сколько более старых снимков пробовать при непригодном свежем (ФТ-127).
    pub fallback_depth: usize,
    /// Через сколько дней снимок считается устаревшим для повторного прогона (ФТ-130).
    pub refresh_days: u32,
    /// Проверить свежесть всех файлов, не глядя на журнал (ФТ-130).
    pub refresh: bool,
    /// Окно адресов для страниц: запрос `from=` в индексе (ФТ-128).
    pub since: Option<String>,
    /// Время старта инвентаризации (unix-секунды): верхняя граница окна индекса (ФТ-151).
    watermark: u64,
    /// Предел страниц обхода индекса (ФТ-149): 0 — без предела.
    index_max_pages: usize,
    /// Индекс пришлось остановить по пределу страниц или по размеру выборки (ФТ-149).
    index_truncated: bool,
    /// Сколько адресов отдал индекс до применения пользовательского --limit.
    index_total: usize,
    /// Сколько страниц индекса реально обойдено (ФТ-149).
    index_pages: usize,
    /// Сколько адресов отброшено пользовательским --limit (для сверки ФТ-154).
    limit_skipped: usize,
    /// Предел добора по ссылкам (ФТ-128); 0 — выключено.
    pub rescue_limit: usize,
    /// Бюджет времени на добор по ссылкам, секунд (0 — без бюджета).
    pub rescue_budget: u64,
    /// Через сколько дней повторять адрес, у которого не было пригодного снимка;
    /// 0 — повторять всегда (ФТ-135).
    pub retry_missing_days: u32,
    /// Порог «пустой страницы» в байтах (ФТ-127).
    pub stub_bytes: usize,
    /// Адреса, добранные по ссылкам в этом прогоне.
    pub rescued: AtomicUsize,
    shared: Arc<Shared>,
    started: Instant,
    items: Vec<Item>,
    inv_failed: bool,
    /// Сколько адресов добрано на этапе сборки (ФТ-153); в 3.6.0 — только
    /// задел: пересборка затронутых страниц появится вместе с build(dirty).
    added_build: usize,
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
    /// Цели ссылок, собранные при переписывании: по списку на страницу.
    /// Проверка идёт по ним, без второго чтения и повторного разбора страниц.
    /// Списки разделяются с журналом через `Arc`: копировать их не нужно.
    ref_targets: Mutex<Vec<Arc<Vec<String>>>>,
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
    /// `Arc`: один список живёт и в журнале, и в проверке, копий нет.
    targets: Arc<Vec<String>>,
    /// Файл считается страницей (html и не бинарный).
    page: bool,
    /// Под именем страницы лежит не страница (картинка, PDF): повторный прогон
    /// должен давать тот же счётчик, иначе «не страница» обнуляется.
    foreign: bool,
    /// Фактическая дата снимка (ФТ-129): показывается в отчёте.
    snapshot: String,
    /// Когда файл получен или проверен (секунды Unix): по этому времени
    /// решается, перекачивать ли его при повторном прогоне (ФТ-130).
    checked: u64,
}

impl Engine {
    pub fn new(site: &str, out: &Path, workers: usize, limit: usize) -> Option<Self> {
        let (host, _note) = sanitize_site(site)?;
        // Каталог вывода всегда абсолютный: файловые операции Windows не умеют
        // длинный префикс \?\ на относительном пути, а очистка копии через него.
        let out = absolute_dir(out);
        // Д-18: раздельные пределы ожидания (ПР-5).
        //
        // Важное уточнение по ureq 3.5.1 (проверено по исходникам крейта 12.11.2026):
        // `timeout_global` покрывает ВСЮ операцию вместе с чтением тела, поэтому
        // прежние 60 с означали «файл должен успеть целиком за минуту» — крупные
        // файлы обрывались. Для файлов теперь:
        //   * connect 15 с — установка соединения;
        //   * recv_response TIMEOUT_FILE (60 с) — ожидание заголовков;
        //   * per_read 30 с — перезапускается на каждом чтении, то есть ловит
        //     зависшее соединение, но не ограничивает общую длительность;
        //   * тело без общего предела — размер файла заранее неизвестен.
        // Для индекса ответ ограничен по размеру, поэтому там общий предел:
        // заголовки 60 с + тело TIMEOUT_INDEX (120 с) = TIMEOUT_TOTAL (180 с).
        let file_config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_recv_response(Some(Duration::from_secs(TIMEOUT_FILE)))
            .timeout_per_read(Some(Duration::from_secs(30)))
            .build();
        let index_config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_recv_response(Some(Duration::from_secs(TIMEOUT_FILE)))
            .timeout_recv_body(Some(Duration::from_secs(TIMEOUT_INDEX)))
            .timeout_per_read(Some(Duration::from_secs(30)))
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
            snapshot: SnapshotMode::Newest,
            fallback_depth: DEFAULT_FALLBACK_DEPTH,
            refresh_days: DEFAULT_REFRESH_DAYS,
            refresh: false,
            since: None,
            watermark: 0,
            index_max_pages: DEFAULT_INDEX_MAX_PAGES,
            index_truncated: false,
            index_total: 0,
            index_pages: 0,
            limit_skipped: 0,
            rescue_limit: DEFAULT_RESCUE_LIMIT,
            rescue_budget: DEFAULT_RESCUE_BUDGET,
            retry_missing_days: DEFAULT_RETRY_MISSING_DAYS,
            stub_bytes: DEFAULT_STUB_BYTES,
            rescued: AtomicUsize::new(0),
            shared: Arc::new(Shared {
                agent: ureq::Agent::new_with_config(file_config),
                agent_index: ureq::Agent::new_with_config(index_config),
                match_type: Mutex::new(MatchType::Domain),
                agent_archive: ARCHIVE_PRIMARY.to_string(),
                archive_alt: ARCHIVE_FALLBACK.to_string(),
                archive_switched: AtomicBool::new(false),
                archive_switch_logged: AtomicBool::new(false),
                mirror: out.join("mirror"),
                gate: Gate::new(MIN_THREADS),
                cancel: Arc::new(AtomicBool::new(false)),
                counters: Counters::default(),
                paused: Arc::new(AtomicBool::new(false)),
                snapshot: Mutex::new(SnapshotMode::Newest),
                stub_bytes: AtomicUsize::new(DEFAULT_STUB_BYTES),
                fallback_depth: AtomicUsize::new(DEFAULT_FALLBACK_DEPTH),
                refresh: Mutex::new(HashSet::new()),
                no_snapshot: Mutex::new(HashMap::new()),
                snapshots: Mutex::new(HashMap::new()),
                types: Mutex::new(TypeCounts::default()),
                cb: std::sync::OnceLock::new(),
                dedup: Mutex::new(HashMap::new()),
                shared_paths: Mutex::new(HashMap::new()),
                pause_until: Mutex::new(Instant::now()),
                rate_nanos: AtomicU64::new(0),
                rate_next: Mutex::new(Instant::now()),
                content_types: Mutex::new(HashMap::new()),
                foreign: AtomicUsize::new(0),
                foreign_logged: AtomicUsize::new(0),
            }),
            started: Instant::now(),
            items: Vec::new(),
            inv_failed: false,
            added_build: 0,
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
        // Обработчик ставится один раз: Shared создаётся с OnceLock, поэтому
        // пересоздавать его и переносить поля вручную больше не нужно
        // (2.3 архитектурного разбора).
        let _ = self.shared.cb.set(Arc::new(cb));
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
    /// Режим выбора снимка (ФТ-126).
    pub fn set_snapshot(&mut self, mode: SnapshotMode) {
        self.snapshot = mode.clone();
        if let Ok(mut shared) = self.shared.snapshot.lock() {
            *shared = mode;
        }
    }

    /// Глубина добора к более старым снимкам (ФТ-127); 0 — выключить добор.
    pub fn set_fallback_depth(&mut self, depth: usize) {
        self.fallback_depth = depth.min(MAX_FALLBACK_DEPTH);
        self.shared
            .fallback_depth
            .store(self.fallback_depth, Ordering::Relaxed);
    }

    /// Срок свежести снимка для повторного прогона, в днях (ФТ-130).
    pub fn set_refresh_days(&mut self, days: u32) {
        self.refresh_days = days;
    }

    /// Проверить свежесть всех файлов заново (ФТ-130).
    pub fn set_refresh(&mut self, refresh: bool) {
        self.refresh = refresh;
    }

    /// Окно адресов для страниц (ФТ-128).
    pub fn set_since(&mut self, since: Option<String>) {
        self.since = since;
    }

    /// Предел добора по ссылкам (ФТ-128).
    /// Предел страниц обхода индекса (ФТ-149): 0 — без предела.
    pub fn set_index_max_pages(&mut self, pages: usize) {
        self.index_max_pages = pages;
    }

    pub fn set_rescue_limit(&mut self, limit: usize) {
        self.rescue_limit = limit;
    }

    /// Бюджет времени на добор по ссылкам, секунд (0 — без бюджета).
    pub fn set_rescue_budget(&mut self, seconds: u64) {
        self.rescue_budget = seconds;
    }

    /// Через сколько дней повторять адреса без пригодного снимка (0 — всегда).
    pub fn set_retry_missing_days(&mut self, days: u32) {
        self.retry_missing_days = days;
    }

    /// Порог «пустой страницы» (ФТ-127).
    pub fn set_stub_bytes(&mut self, bytes: usize) {
        self.stub_bytes = bytes;
        self.shared.stub_bytes.store(bytes, Ordering::Relaxed);
    }

    /// Признак паузы: общий для ядра и интерфейса. В консольной сборке паузы нет.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn pause_token(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shared.paused)
    }

    pub fn cancel_token(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shared.cancel)
    }

    #[allow(dead_code)] // публичный API: используется тестами и внешними вызовами
    pub fn cancel(&self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
    }

    fn emit(&self, stage: &'static str, overall: f32, text: &str, _force: bool) {
        let Some(cb) = self.shared.cb.get() else {
            return;
        };
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
    /// Путь к списку адресов без снимка.
    fn no_snapshot_path(&self) -> PathBuf {
        self.data_dir.join("no-snapshot.tsv")
    }

    /// Путь к справочнику дат снимков.
    fn snapshots_path(&self) -> PathBuf {
        self.data_dir.join("snapshots.tsv")
    }

    /// Прочитать даты снимков: `путь`, табуляция, отпечаток снимка.
    ///
    /// Журнал сборки описывает и логику переписывания, поэтому при смене версии
    /// программы он отвергается целиком — вместе с датами снимков. Справочник
    /// хранит только даты и переживает обновление: иначе в отчёте «Годы снимков»
    /// пустеет после каждого выпуска.
    fn load_snapshots(&self) {
        let Ok(text) = fs::read_to_string(self.snapshots_path()) else {
            return;
        };
        let Ok(mut map) = self.shared.snapshots.lock() else {
            return;
        };
        for line in text.lines() {
            if let Some((path, stamp)) = line.split_once('\t')
                && !path.is_empty()
                && stamp.len() >= 8
            {
                map.entry(path.to_string())
                    .or_insert_with(|| stamp.to_string());
            }
        }
    }

    /// Сохранить даты снимков: из загрузки этого прогона и из журнала сборки.
    fn save_snapshots(&self) {
        let mut all: HashMap<String, String> = match self.shared.snapshots.lock() {
            Ok(map) => map.clone(),
            Err(_) => return,
        };
        if let Ok(journal) = self.journal.lock() {
            for (rel, entry) in journal.iter() {
                if entry.snapshot.len() >= 8 {
                    all.insert(rel.clone(), entry.snapshot.clone());
                }
            }
        }
        if all.is_empty() {
            return;
        }
        let mut keys: Vec<&String> = all.keys().collect();
        keys.sort();
        let mut out = String::with_capacity(all.len() * 80);
        for key in keys {
            out.push_str(key);
            out.push('\t');
            out.push_str(&all[key]);
            out.push('\n');
        }
        let _ = fs::create_dir_all(&self.data_dir);
        let path = self.snapshots_path();
        let tmp = path.with_extension("tsv.part");
        if fs::write(&tmp, out).is_ok() {
            let _ = fs::rename(&tmp, &path);
        }
    }

    /// Прочитать список адресов без снимка: `url`, табуляция, время проверки.
    fn load_no_snapshot(&self) -> HashMap<String, u64> {
        let Ok(text) = fs::read_to_string(self.no_snapshot_path()) else {
            return HashMap::new();
        };
        text.lines()
            .filter_map(|line| {
                let (url, stamp) = line.split_once('\t')?;
                Some((url.to_string(), stamp.trim().parse().ok()?))
            })
            .collect()
    }

    /// Сохранить список, выбросив записи старше срока повтора.
    fn save_no_snapshot(&self) {
        let Ok(fresh) = self.shared.no_snapshot.lock() else {
            return;
        };
        if fresh.is_empty() {
            return;
        }
        let mut all = self.load_no_snapshot();
        for (url, stamp) in fresh.iter() {
            all.insert(url.clone(), *stamp);
        }
        let ttl = u64::from(self.retry_missing_days) * 86_400;
        let now = now_secs();
        let mut out = String::with_capacity(all.len() * 80);
        for (url, stamp) in all {
            if self.retry_missing_days > 0 && now.saturating_sub(stamp) > ttl.saturating_mul(4) {
                continue;
            }
            out.push_str(&url);
            out.push('\t');
            out.push_str(&stamp.to_string());
            out.push('\n');
        }
        let _ = fs::create_dir_all(&self.data_dir);
        let _ = fs::write(self.no_snapshot_path(), out);
    }

    /// Файлы, чей снимок старше `--refresh-days` или которые нужно проверить заново (ФТ-130).
    /// Работает только в режиме «самый свежий»: в режиме `oldest` журнал ведёт себя как в 3.4.0.
    fn stale_paths(&self) -> HashSet<String> {
        if self.snapshot == SnapshotMode::Oldest && !self.refresh {
            return HashSet::new();
        }
        let journal = self.load_journal();
        if journal.is_empty() {
            return HashSet::new();
        }
        let ttl_secs = if self.refresh {
            0
        } else {
            u64::from(self.refresh_days) * 86_400
        };
        let now = now_secs();
        journal
            .into_iter()
            .filter(|(path, entry)| {
                if !self.mirror.join(path).exists() {
                    return false;
                }
                // Срок считается от последней проверки, а не от даты снимка:
                // у мёртвого сайта свежих снимков нет, но проверять их всё равно надо.
                now.saturating_sub(entry.checked) >= ttl_secs
            })
            .map(|(path, _)| path)
            .collect()
    }

    /// Запрос списка адресов: одна строка на адрес, самый ранний снимок, длина записи.
    fn quick_query(&self, from: Option<&str>) -> String {
        let mut extra = String::from(
            "output=json&fl=timestamp,original,mimetype,statuscode,length,digest&filter=statuscode:200&collapse=urlkey",
        );
        extra.push_str("&limit=");
        extra.push_str(&QUICK_LIMIT.to_string());
        if let Some(from) = from {
            extra.push_str("&from=");
            extra.push_str(from);
        }
        if let Some(to) = self.window_to() {
            extra.push_str("&to=");
            extra.push_str(&to);
        }
        self.cdx_query(&extra)
    }

    /// Страница индекса в резервном постраничном обходе.
    fn paged_query(&self, page: usize) -> String {
        let mut extra = format!(
            "output=json&fl=timestamp,original,mimetype,statuscode,length,digest&filter=statuscode:200&collapse=urlkey&limit={PAGE_LIMIT}&page={page}"
        );
        if let Some(from) = &self.since {
            extra.push_str("&from=");
            extra.push_str(from);
        }
        if let Some(to) = self.window_to() {
            extra.push_str("&to=");
            extra.push_str(&to);
        }
        self.cdx_query(&extra)
    }

    /// Свободное место на диске, где лежит папка результата (Д-15).
    fn free_space(&self) -> Option<u64> {
        free_bytes(&self.out)
    }

    /// Потребность в месте: длина записей индекса (сжатые байты) с запасом на распаковку
    /// и вторую копию дерева.
    fn estimate_bytes(&self) -> u64 {
        let sum: u64 = self.items.iter().map(|item| item.len).sum();
        sum.saturating_mul(3)
    }

    /// Удалить временные файлы прошлых прогонов (Д-16).
    ///
    /// `.part` появляется только в зеркале (недокачанный файл) и в данных (журнал),
    /// поэтому дерево обходится один раз, а зеркало — заодно с этим: список файлов
    /// идёт в сборку, второй обход не нужен.
    fn cleanup_temp(&self) -> usize {
        let mut removed = 0usize;
        if self.mirror.exists() {
            let mirror_files = walk_files(&self.mirror);
            for path in &mirror_files {
                if is_temp_file(path) && fs::remove_file(long_path(path)).is_ok() {
                    removed += 1;
                }
            }
            let count = mirror_files.len();
            *self.mirror_cache.lock().unwrap() = Some(Arc::new(mirror_files));
            self.mirror_files.store(count, Ordering::Relaxed);
        }
        if self.data_dir.exists() {
            for path in walk_files(&self.data_dir) {
                if is_temp_file(&path) && fs::remove_file(long_path(&path)).is_ok() {
                    removed += 1;
                }
            }
        }
        removed
    }

    fn cdx_query(&self, extra: &str) -> String {
        let match_type = match *self.shared.match_type.lock().unwrap() {
            MatchType::Domain => "domain",
            MatchType::Prefix => "prefix",
        };
        format!(
            "{}/cdx/search/cdx?url={}&matchType={}&{}",
            self.shared.archive_base(),
            urlencode(&self.host),
            match_type,
            extra
        )
    }

    /// Текущий режим сопоставления (для сравнения до и после переключения).
    fn match_type_now(&self) -> MatchType {
        *self.shared.match_type.lock().unwrap()
    }

    /// Переключить режим сопоставления (ФТ-149): на 504 домен не отдаётся,
    /// а префикс отвечает. Пишем в журнал один раз, чтобы не засорять вывод.
    fn switch_match_type(&self, reason: &str) {
        let mut current = self.shared.match_type.lock().unwrap();
        if *current == MatchType::Domain {
            *current = MatchType::Prefix;
            self.shared.log(crate::tr::pick(
                format!("CDX: режим domain недоступен ({reason}), переключаюсь на prefix"),
                format!("CDX: domain mode unavailable ({reason}), switching to prefix"),
            ));
        }
    }

    /// Верхняя граница окна индекса (ФТ-151): время старта инвентаризации.
    ///
    /// Нужна, чтобы «полнота» была проверяемой: индекс живёт, и один и тот же
    /// запрос в разные минуты отдаёт разное число строк.
    fn window_to(&self) -> Option<String> {
        (self.watermark > 0).then(|| stamp_from_secs(self.watermark))
    }

    fn inventory(&mut self) -> Result<(), String> {
        // Водяной знак фиксируется до первого запроса: всё, что появится в архиве
        // позже, добрается отдельным проходом (ФТ-151, ФТ-152).
        self.watermark = now_secs();
        self.index_truncated = false;
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
            Ok(fetched) if fetched.status == 200 => {
                if let Some(n) = parse_pages(&String::from_utf8_lossy(&fetched.body)) {
                    pages = n;
                }
            }
            Ok(fetched) => self
                .shared
                .log(format!("CDX: оценка недоступна (HTTP {})", fetched.status)),
            Err(err) => self.shared.log(crate::tr::pick(
                format!("CDX: оценка — ошибка {err}"),
                format!("CDX: estimate failed: {err}"),
            )),
        }

        // Быстрый запрос выполняется ВСЕГДА: постраничный режим (`page=N`) у
        // web.archive.org поддерживается не для всех сайтов и бывает пустым,
        // а запрос с limit=50000 отдаёт те же данные.
        // Строки индекса свёртываются в карту сразу, а не копятся в Vec:
        // 200 страниц по 20 000 строк давали до 4 млн строк в памяти (2.4).
        let mut best: HashMap<String, (String, String, String, u64, String)> = HashMap::new();
        // Сколько строк разобрано: для проверки усечения и для отчёта.
        let mut parsed_rows = 0usize;
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
            // Запрос собирается на каждой попытке: при 504 режим сопоставления
            // переключается, и повтор должен уйти уже в новом режиме (ФТ-149).
            let quick = self.quick_query(self.since.as_deref());
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
                self.shared.fetch_stream(&quick, &mut on_index)
            };
            match result {
                Ok(fetched) if fetched.status == 200 => {
                    let chunk = parse_rows(&fetched.body);
                    parsed_rows = chunk.len();
                    for row in &chunk {
                        fold_row(&mut best, &self.host, row, true);
                    }
                    if !chunk.is_empty() {
                        break;
                    }
                }
                Ok(fetched) => {
                    let code = fetched.status;
                    let retry = fetched.retry_after;
                    self.shared.log(crate::tr::pick(
                        format!("CDX: список — HTTP {code}"),
                        format!("CDX: list — HTTP {code}"),
                    ));
                    // 504 на крупном домене — отказ режима domain: префикс отвечает.
                    if code == 504 {
                        self.switch_match_type("HTTP 504");
                    }
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
        if !best.is_empty() {
            // Список получен, пусть и со второй попытки: отказом это не считаем.
            // Раньше признак ставился на первой сетевой ошибке и не снимался,
            // поэтому отчёт показывал «адресов 1638, индекс не получен».
            self.inv_failed = false;
        }
        if best.is_empty() {
            self.shared.log(crate::tr::t(
                "CDX: быстрый запрос пуст — постраничный обход",
                "CDX: quick query empty — switching to paging",
            ));
            pages = pages.max(2);
        } else if parsed_rows + QUICK_SLACK >= QUICK_LIMIT {
            // Оказаться у самой границы можно и на полном индексе: тогда обход
            // просто пройдёт целиком и снимет признак усечения (ниже).
            self.index_truncated = true;
            self.shared.log(crate::tr::pick(
                format!("CDX: список обрезан на {QUICK_LIMIT} адресах"),
                format!("CDX: address list truncated at {QUICK_LIMIT}"),
            ));
        } else {
            // Быстрый запрос вернул весь индекс: обход не нужен, покрытие полное.
            // Иначе в отчёте стояло бы «0 страниц» при полном индексе.
            self.index_pages = pages.max(1);
        }
        // Оценка страниц бывает недоступна (503/504 на showNumPages): тогда pages = 1,
        // но обход всё равно нужен — иначе обрезанный список останется обрезанным.
        let estimate_known = pages > 1;
        if best.is_empty() || self.index_truncated {
            let max_pages = if self.index_max_pages == 0 {
                if estimate_known { pages } else { usize::MAX }
            } else if estimate_known {
                pages.min(self.index_max_pages).max(1)
            } else {
                self.index_max_pages
            };
            let shown_pages = if max_pages == usize::MAX {
                pages.max(2)
            } else {
                max_pages
            };
            if estimate_known && max_pages < pages {
                self.index_truncated = true;
                self.shared.log(crate::tr::pick(
                    format!(
                        "CDX: индекс обрезан: {max_pages} страниц из {pages} (--index-max-pages)"
                    ),
                    format!(
                        "CDX: index truncated: {max_pages} pages of {pages} (--index-max-pages)"
                    ),
                ));
            }
            let mut page = 0usize;
            let mut attempts = 0u32;
            let mut restarted = false;
            // Обход закончился на странице короче PAGE_LIMIT — индекс собран целиком.
            let mut finished = false;
            while page < max_pages {
                if self.shared.cancel.load(Ordering::Relaxed) {
                    return Err("cancelled".to_string());
                }
                let query = self.paged_query(page);
                let page_no = page;
                self.emit(
                    "inventory",
                    4.0 + 4.0 * page_no as f32 / shown_pages as f32,
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
                            4.0 + 4.0 * (page_no as f32 + inner as f32) / shown_pages as f32;
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
                    Ok(fetched) if fetched.status == 200 => {
                        attempts = 0;
                        let chunk = parse_rows(&fetched.body);
                        let got = chunk.len();
                        // Пустая первая страница при непустом списке — не конец
                        // индекса, а отказ режима: на крупных доменах archive.org
                        // отдаёт по страницам только prefix (проверено на nginx.org,
                        // где domain отдал заголовок без строк).
                        if got == 0 && page == 0 {
                            // Пустая страница — не конец индекса. Если есть куда
                            // переключить режим, начинаем заново; если режим уже
                            // prefix, обход недоступен и об этом надо сказать прямо.
                            let before = self.match_type_now();
                            self.switch_match_type("пустая первая страница");
                            if self.match_type_now() != before && !restarted {
                                restarted = true;
                                best.clear();
                                attempts = 0;
                                self.index_pages = 0;
                                continue;
                            }
                            self.shared.log(crate::tr::t(
                                "CDX: постраничный обход не отдаёт записей — индекс остаётся обрезанным",
                                "CDX: paging returns nothing — the index stays truncated",
                            ));
                            break;
                        }
                        for row in &chunk {
                            fold_row(&mut best, &self.host, row, true);
                        }
                        page += 1;
                        self.index_pages = page;
                        if got > 0 && got < PAGE_LIMIT {
                            // Конец доказывает только непустая короткая страница:
                            // пустой ответ означает отказ, а не конец индекса.
                            finished = true;
                            break;
                        }
                        continue;
                    }
                    Ok(fetched) => {
                        let code = fetched.status;
                        let retry = fetched.retry_after;
                        if code == 504 {
                            // Режим сопоставления влияет на разбиение на страницы:
                            // продолжать в новом режиме нельзя — собранные страницы
                            // пришлось бы смешать с чужими. Начинаем обход заново.
                            let before = self.match_type_now();
                            self.switch_match_type("HTTP 504 на странице");
                            if self.match_type_now() != before && !restarted {
                                restarted = true;
                                best.clear();
                                page = 0;
                                attempts = 0;
                                continue;
                            }
                        }
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
            if finished {
                // Обход прошёл до конца: прежнее усечение больше не действует,
                // а прежние сетевые отказы на страницах уже не важны.
                self.index_truncated = false;
                self.inv_failed = false;
            } else if self.index_truncated && !self.inv_failed {
                self.shared.log(crate::tr::pick(
                    format!(
                        "CDX: обход не дал полного индекса (страниц {}) — копия останется неполной",
                        self.index_pages
                    ),
                    format!(
                        "CDX: paging did not cover the index ({} pages) — the copy will stay incomplete",
                        self.index_pages
                    ),
                ));
            }
        }

        // Окно по дате (ФТ-128): страницы берутся из окна, ресурсы — из полной истории,
        // иначе современная страница останется без старых картинок и стилей.
        if let Some(since) = self.since.clone() {
            self.shared.log(crate::tr::pick(
                format!("адреса ограничены окном с {since}: страницы — из окна, ресурсы — вся история"),
                format!("address list limited from {since}: pages from the window, assets from full history"),
            ));
            let full = self.quick_query(None);
            if let Ok(fetched) = self.shared.fetch_stream(&full, &mut |_, _| {})
                && fetched.status == 200
            {
                // Из полной истории добираются только ресурсы: страницы уже взяты
                // из окна, поэтому `from_window = false`.
                for row in parse_rows(&fetched.body) {
                    fold_row(&mut best, &self.host, &row, false);
                }
            }
        }

        // Один путь копии — одна версия (ФТ-131): из адресов, дающих один файл,
        // остаётся самый свежий снимок; остальные не скачиваются вовсе.
        let mut by_path: HashMap<String, Item> = HashMap::with_capacity(best.len());
        let mut collisions = 0usize;
        for (url, (ts, orig, mime, len, digest)) in best {
            let item = Item {
                path: paths::url_to_local_path_in(&url, &self.host),
                kind: paths::file_type(&url),
                url,
                ts,
                orig,
                mime,
                len,
                digest,
                same_as: String::new(),
            };
            match by_path.get(&item.path) {
                Some(prev) if keep_previous(&item, prev) => {
                    collisions += 1;
                }
                Some(_) => {
                    collisions += 1;
                    by_path.insert(item.path.clone(), item);
                }
                None => {
                    by_path.insert(item.path.clone(), item);
                }
            }
        }
        self.shared
            .counters
            .collisions
            .store(collisions, Ordering::Relaxed);
        if collisions > 0 {
            self.shared.log(crate::tr::pick(
                format!("одна страница на несколько адресов: отброшено {collisions}"),
                format!("one file per several addresses: {collisions} dropped"),
            ));
        }

        let mut items: Vec<Item> = by_path.into_values().collect();
        items.sort_by(|a, b| a.url.cmp(&b.url));
        // Адреса, у которых снимка нет, повторяются не каждый прогон (ФТ-135).
        if self.retry_missing_days > 0 {
            let known = self.load_no_snapshot();
            if !known.is_empty() {
                let ttl = u64::from(self.retry_missing_days) * 86_400;
                let now = now_secs();
                let before = items.len();
                items.retain(|item| match known.get(&item.url) {
                    Some(checked) => now.saturating_sub(*checked) >= ttl,
                    None => true,
                });
                let skipped = before - items.len();
                if skipped > 0 {
                    self.shared.log(crate::tr::pick(
                        format!(
                            "без снимка: {} адресов пропущено до повтора через {} дней",
                            skipped, self.retry_missing_days
                        ),
                        format!(
                            "no snapshot: {} addresses skipped until retry in {} days",
                            skipped, self.retry_missing_days
                        ),
                    ));
                }
            }
        }
        // Число адресов индекса фиксируется до пользовательского --limit (ФТ-154):
        // иначе при работе без лимита счётчик оставался нулевым.
        self.index_total = items.len();
        if self.limit > 0 {
            self.limit_skipped = items.len().saturating_sub(self.limit);
            items.truncate(self.limit);
        }
        {
            let mut types = self.shared.types.lock().unwrap();
            for item in &items {
                types.slot(item.kind)[1] += 1;
            }
        }
        fs::create_dir_all(&self.data_dir).map_err(|e| e.to_string())?;
        self.write_inventory(&items)?;
        self.items = items;
        if self.items.is_empty() {
            return Err(if self.inv_failed {
                "archive unavailable".to_string()
            } else {
                "archive has no pages for this address".to_string()
            });
        }
        self.shared.log(crate::tr::pick(
            format!(
                "индекс: страниц {}, адресов {}, {}",
                self.index_pages,
                self.index_total,
                if self.inv_failed {
                    "не получен"
                } else if self.index_truncated {
                    "обрезан"
                } else {
                    "полон"
                }
            ),
            format!(
                "index: {} pages, {} addresses, {}",
                self.index_pages,
                self.index_total,
                if self.inv_failed {
                    "failed"
                } else if self.index_truncated {
                    "truncated"
                } else {
                    "complete"
                }
            ),
        ));
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
        // Что перекачать заново: снимок старше срока свежести (ФТ-130).
        let refresh = self.stale_paths();
        if !refresh.is_empty() {
            self.shared.log(crate::tr::pick(
                format!(
                    "свежесть: {} файлов старше {} дней, перекачиваю",
                    refresh.len(),
                    self.refresh_days
                ),
                format!(
                    "freshness: {} files older than {} days, re-downloading",
                    refresh.len(),
                    self.refresh_days
                ),
            ));
            if let Ok(mut set) = self.shared.refresh.lock() {
                *set = refresh;
            }
        }
        let mut todo: Vec<Item> = Vec::new();
        let mut already = 0usize;
        for item in &self.items {
            let full = self.mirror.join(&item.path);
            match fs::metadata(&full) {
                Ok(meta) if meta.len() > 0 => {
                    let stale = self
                        .shared
                        .refresh
                        .lock()
                        .map(|set| set.contains(&item.path))
                        .unwrap_or(false);
                    if stale {
                        todo.push(item.clone());
                    } else {
                        already += 1;
                        self.shared.types.lock().unwrap().slot(item.kind)[0] += 1;
                    }
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

        // Файлы появятся в зеркале: список, снятый при уборке, больше не годится.
        *self.mirror_cache.lock().unwrap() = None;
        let queue = Arc::new(Queue::new(Self::order_interleaved(&todo), already));
        let stop = Arc::new(AtomicBool::new(false));
        // Адреса, добавленные добором: после загрузки войдут в список прогона.
        let added_log: Arc<Mutex<Vec<Item>>> = Arc::new(Mutex::new(Vec::new()));

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
            let queue = Arc::clone(&queue);
            let stop = Arc::clone(&stop);
            let cb = shared.cb.get().cloned();
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
                            overall: 10.0 + 75.0 * done as f32 / queue.planned().max(1) as f32,
                            text: crate::tr::pick(
                                format!("Скачано {done} из {}", queue.planned()),
                                format!("Downloaded {done} of {}", queue.planned()),
                            ),
                            done,
                            total: queue.planned(),
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
                                && queue.planned() > done
                                && !shared.cancel.load(Ordering::Relaxed)
                            {
                                ((queue.planned().saturating_sub(done)) as f64
                                    / (done as f64 / elapsed))
                                    as u64
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

        // Добор на ходу (ФТ-152): индекс живёт, поэтому раз в две минуты
        // спрашиваем всё, что появилось после водяного знака, и ставим в очередь.
        let recheck = {
            let shared = Arc::clone(&self.shared);
            let queue = Arc::clone(&queue);
            let added_log = Arc::clone(&added_log);
            let host = self.host.clone();
            let from = stamp_from_secs(self.watermark);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                loop {
                    // Сон дробится на секунды: иначе join ждал бы его конца и
                    // добавлял до двух минут к прогону (замер: 183 с вместо ~60).
                    for _ in 0..120 {
                        if stop.load(Ordering::Relaxed) || shared.cancel.load(Ordering::Relaxed) {
                            return;
                        }
                        std::thread::sleep(Duration::from_secs(1));
                    }
                    let extra = format!(
                        "output=json&fl=timestamp,original,mimetype,statuscode,length,digest&filter=statuscode:200&collapse=urlkey&from={from}"
                    );
                    let query = format!(
                        "{}/cdx/search/cdx?url={}&matchType=domain&{}",
                        shared.archive_base(),
                        urlencode(&host),
                        extra
                    );
                    let Ok(fetched) = shared.fetch_stream(&query, &mut |_, _| {}) else {
                        continue;
                    };
                    if fetched.status != 200 {
                        continue;
                    }
                    let mut best: HashMap<String, (String, String, String, u64, String)> =
                        HashMap::new();
                    for row in parse_rows(&fetched.body) {
                        fold_row(&mut best, &host, &row, true);
                    }
                    let mut added = 0usize;
                    for (url, (ts, orig, mime, len, digest)) in best {
                        let item = Item {
                            path: paths::url_to_local_path_in(&url, &host),
                            kind: paths::file_type(&url),
                            url,
                            ts,
                            orig,
                            mime,
                            len,
                            digest,
                            same_as: String::new(),
                        };
                        if queue.push(item.clone()) {
                            // Список адресов прогона пополняется вместе с очередью:
                            // без этого ok и файлы копии росли бы, а items, инвентарь
                            // и unique_paths — нет, и отчёт противоречил бы себе.
                            if let Ok(mut log) = added_log.lock() {
                                log.push(item);
                            }
                            added += 1;
                        }
                    }
                    if added > 0 {
                        shared
                            .counters
                            .added_live
                            .fetch_add(added, Ordering::Relaxed);
                        shared.log(crate::tr::pick(
                            format!("добор: в индекс добавилось {added} адресов"),
                            format!("live: {added} new addresses from the index"),
                        ));
                    }
                }
            })
        };

        let mut handles = Vec::new();
        for _ in 0..self.workers {
            let shared = Arc::clone(&self.shared);
            let queue = Arc::clone(&queue);
            handles.push(std::thread::spawn(move || {
                // Адрес берётся из очереди с ожиданием: пока идёт добор, очередь
                // может пополниться, и воркер не должен выходить раньше времени.
                while let Some(item) = queue.pop() {
                    if shared.cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    shared.gate.acquire();
                    shared.fetch_one(&item);
                    shared.gate.release();
                }
            }));
        }

        // Ждём, пока вся работа выполнена: очередь может пополняться добором,
        // поэтому закрывать её сразу нельзя — добавленные адреса потерялись бы.
        loop {
            std::thread::sleep(Duration::from_millis(500));
            if self.shared.cancel.load(Ordering::Relaxed) {
                break;
            }
            if self.shared.counters.done.load(Ordering::Relaxed) >= queue.planned() {
                break;
            }
        }
        stop.store(true, Ordering::Relaxed);
        // Добор останавливается на ближайшей секунде и закрывает очередь;
        // воркеры выходят по закрытию, а не по «индекс за концом».
        let _ = recheck.join();
        queue.close();
        for handle in handles {
            let _ = handle.join();
        }
        // Добранные адреса становятся частью прогона: инвентарь и отчёт считаются
        // по одному списку, поэтому числа не расходятся (ФТ-154).
        let live: Vec<Item> = added_log.lock().map(|log| log.clone()).unwrap_or_default();
        if !live.is_empty() {
            let known: HashSet<String> = self.items.iter().map(|i| i.path.clone()).collect();
            let fresh = live
                .into_iter()
                .filter(|i| !known.contains(&i.path))
                .count();
            let mut all = std::mem::take(&mut self.items);
            all.extend(added_log.lock().map(|log| log.clone()).unwrap_or_default());
            all.sort_by(|a, b| a.url.cmp(&b.url));
            self.items = all;
            self.index_total += fresh;
        }
        let _ = progress_thread.join();
        let _ = controller.join();
        self.save_content_types();
        self.save_no_snapshot();
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

        // Один шаблон на атрибуты ссылок и наборы источников (ФТ-148): раньше
        // документ проходился дважды. Семантика сохранена: наборы источников
        // разбираются только в кавычках, без кавычек значение считается адресом.
        let link_rx = Regex::new(
            r#"(?i)(href|src|data-src|data-bg|poster|action|data-original|srcset|data-srcset)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'<>`=]+))"#,
        )
        .unwrap();
        let css_rx = Regex::new(r#"url\(\s*["']?([^"')]+)["']?\s*\)"#).unwrap();
        let base_rx = Regex::new(r"(?i)<base\b[^>]*>").unwrap();
        let site_rx = Regex::new(&format!(
            r#"(?i)^(?:https?:)?//(?:www\.)?{}(?::\d+)?(/[^"'\s<>()\\]*)"#,
            regex::escape(&self.host)
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

                    // Дата снимка файла: записывается в журнал, показывается в отчёте (ФТ-129).
                    let snapshot = self
                        .shared
                        .snapshots
                        .lock()
                        .ok()
                        .and_then(|map| map.get(&rel).cloned())
                        .or_else(|| journal.get(&rel).map(|prev| prev.snapshot.clone()))
                        .unwrap_or_default();
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
                                    targets: Arc::new(Vec::new()),
                                    page: false,
                                    foreign: is_page,
                                    snapshot,
                                    checked: now_secs(),
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
                            // Раньше здесь любой абсолютный адрес своего домена
                            // обрезался до пути: ломались адреса API и строки
                            // сравнения. Теперь правило то же, что у атрибутов:
                            // переписываем только существующую в копии цель.
                            text = site_rx
                                .replace_all(&text, |caps: &regex::Captures| {
                                    let full = caps.get(0).map(|m| m.as_str()).unwrap_or("");
                                    self.rewrite_ref(full, src_dir, &index, &site_rx)
                                })
                                .into_owned();
                        } else {
                            text = link_rx
                                .replace_all(&text, |caps: &regex::Captures| {
                                    let attr = &caps[1];
                                    // Кавычки: 2 — двойные, 3 — одинарные, 4 — без кавычек.
                                    let quoted = caps.get(4).is_none();
                                    let quote = if caps.get(2).is_some() {
                                        "\""
                                    } else if caps.get(3).is_some() {
                                        "'"
                                    } else {
                                        "\""
                                    };
                                    let value = caps
                                        .get(2)
                                        .or_else(|| caps.get(3))
                                        .or_else(|| caps.get(4))
                                        .map(|m| m.as_str())
                                        .unwrap_or("");
                                    let is_set = attr.eq_ignore_ascii_case("srcset")
                                        || attr.eq_ignore_ascii_case("data-srcset");
                                    if is_set && quoted {
                                        let items: Vec<String> = value
                                            .split(',')
                                            .filter(|s| !s.trim().is_empty())
                                            .map(|item| {
                                                let mut bits = item.split_whitespace();
                                                let url = bits.next().unwrap_or("");
                                                let rest: Vec<&str> = bits.collect();
                                                let new_url = self
                                                    .rewrite_ref(url, src_dir, &index, &site_rx);
                                                if rest.is_empty() {
                                                    new_url
                                                } else {
                                                    format!("{new_url} {}", rest.join(" "))
                                                }
                                            })
                                            .collect();
                                        return format!(
                                            "{attr}={quote}{}{quote}",
                                            items.join(", ")
                                        );
                                    }
                                    let new_value =
                                        self.rewrite_ref(value, src_dir, &index, &site_rx);
                                    // Цели считаем по ИТОГОВОЙ ссылке: проверка ссылается
                                    // на то, что реально попало в копию.
                                    collect(&new_value, quoted);
                                    format!("{attr}={quote}{new_value}{quote}")
                                })
                                .into_owned();
                            text = css_rx
                                .replace_all(&text, |caps: &regex::Captures| {
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
                                targets: Arc::new(targets),
                                page: is_page,
                                foreign: false,
                                snapshot,
                                checked: now_secs(),
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
                            targets: Arc::new(Vec::new()),
                            page: false,
                            foreign: false,
                            snapshot,
                            checked: now_secs(),
                        },
                    ))
                })
                .collect()
        });

        // Один обход готовой копии на все нужды: уборка сирот, проверка и отчёт.
        let site_files = Arc::new(walk_files(&self.site_dir));
        self.site_files.store(site_files.len(), Ordering::Relaxed);
        *self.site_cache.lock().unwrap() = Some(Arc::clone(&site_files));
        let mut journal_new = HashMap::with_capacity(results.len());
        let mut targets_all: Vec<Arc<Vec<String>>> = Vec::with_capacity(results.len());
        for (rel, entry) in results {
            if !entry.targets.is_empty() {
                targets_all.push(Arc::clone(&entry.targets));
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
            self.drop_orphans(&files, &site_files);
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

    /// Удаляет из копии файлы, которых нет в зеркале. Список файлов копии уже снят:
    /// второй обход дерева на этом шаге не нужен.
    fn drop_orphans(&self, files: &[PathBuf], site_files: &[PathBuf]) {
        let keep: HashSet<String> = files
            .iter()
            .filter_map(|p| {
                p.strip_prefix(&self.mirror)
                    .ok()
                    .map(|r| r.to_string_lossy().replace('\\', "/").to_lowercase())
            })
            .collect();
        for path in site_files {
            let rel = path
                .strip_prefix(&self.site_dir)
                .unwrap_or(path.as_path())
                .to_string_lossy()
                .replace('\\', "/")
                .to_lowercase();
            if !keep.contains(&rel) {
                let _ = fs::remove_file(long_path(path));
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

    /// Метка журнала: формат, версия сборки, хост, режим снимка и окно адресов.
    /// Смена режима или окна делает журнал непригодным: копия собирается заново.
    fn journal_tag(&self) -> String {
        format!(
            "#journal {} {} {} {} {}",
            JOURNAL_FORMAT,
            env!("CARGO_PKG_VERSION"),
            self.host,
            self.snapshot.tag(),
            self.since.as_deref().unwrap_or("-")
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
                "\",\"size\":{},\"mtime\":{},\"page\":{},\"foreign\":{},\"snapshot\":\"{}\",\"checked\":{},\"targets\":[",
                entry.size,
                entry.mtime,
                entry.page,
                entry.foreign,
                json_escape(&entry.snapshot),
                entry.checked
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

    /// Самый старый и самый новый снимок копии по журналу (ФТ-129).
    fn snapshot_range(&self) -> (String, String) {
        let journal = self.journal.lock().unwrap();
        let mut oldest = String::new();
        let mut newest = String::new();
        for entry in journal.values() {
            if entry.snapshot.len() < 8 {
                continue;
            }
            if oldest.is_empty() || entry.snapshot < oldest {
                oldest = entry.snapshot.clone();
            }
            if newest.is_empty() || entry.snapshot > newest {
                newest = entry.snapshot.clone();
            }
        }
        (stamp_readable(&oldest), stamp_readable(&newest))
    }

    /// Сколько файлов копии старше трёх лет и их доля в процентах (ФТ-129).
    fn old_share(&self, years: u32) -> (usize, usize) {
        let journal = self.journal.lock().unwrap();
        let mut old = 0usize;
        let mut total = 0usize;
        for entry in journal.values() {
            if entry.snapshot.len() < 8 {
                continue;
            }
            total += 1;
            if stamp_age_days(&entry.snapshot).is_some_and(|days| days > years * 365) {
                old += 1;
            }
        }
        (old, total)
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
            None => {
                // Цель не найдена в копии: адрес остаётся абсолютным, но это
                // осознанное решение, а не пропуск — считаем его (ФТ-147).
                self.shared.counters.left.fetch_add(1, Ordering::Relaxed);
                value.to_string()
            }
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
        // Ссылка «parts/b» ведёт на файл «parts/b/index.html»: каталоги собираем
        // заранее одним множеством, иначе на каждую из миллионов ссылок пришлось бы
        // строить строку с «/index.html».
        let dirs: HashSet<&str> = known
            .iter()
            .filter_map(|key| key.strip_suffix("/index.html"))
            .collect();
        // Цели не копируем: на большой копии это десятки мегабайт строк.
        let targets = self.ref_targets.lock().unwrap();
        let refs = AtomicUsize::new(0);
        let broken = AtomicUsize::new(0);
        self.in_pool(|| {
            targets.par_iter().for_each(|keys| {
                for key in keys.iter() {
                    refs.fetch_add(1, Ordering::Relaxed);
                    if !known.contains(key.as_str()) && !dirs.contains(key.as_str()) {
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
        let (oldest, newest) = self.snapshot_range();
        let (old_n, old_total) = self.old_share(3);
        let old_pct = 100.0 * old_n as f32 / old_total.max(1) as f32;
        // Копия не может быть свежее архива: если больше половины файлов старше трёх лет,
        // это надо сказать прямо, а не прятать в таблице (ФТ-129).
        let fresh_warn = if old_total > 0 && old_n * 2 > old_total {
            tr2(
                &format!(
                    "Больше половины файлов копии старше трёх лет ({old_n} из {old_total}): архив не содержит более свежих снимков этих адресов."
                ),
                &format!(
                    "More than half of the copy is older than three years ({old_n} of {old_total}): the archive holds no fresher snapshots for these addresses."
                ),
            )
        } else {
            String::new()
        };
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
            .replace("{{t_snap_new}}", &tr2("Самый новый снимок", "Newest snapshot"))
            .replace("{{t_snap_old}}", &tr2("Самый старый снимок", "Oldest snapshot"))
            .replace("{{t_snap_range}}", &tr2("Годы снимков", "Snapshot years"))
            .replace("{{t_old}}", &tr2("Файлов старше трёх лет", "Files older than three years"))
            .replace("{{t_fallbacks}}", &tr2("Добрано к старым снимкам", "Fell back to older snapshots"))
            .replace("{{t_no_snapshot}}", &tr2("Без пригодного снимка", "No usable snapshot"))
            .replace("{{t_collisions}}", &tr2("Перезаписей путей", "Path collisions"))
            .replace("{{t_rescued}}", &tr2("Добрано по ссылкам", "Rescued by links"))
            .replace(
                "{{t_shared}}",
                &tr2("Взято ссылкой на дубль", "Linked to a duplicate"),
            )
            .replace(
                "{{t_left}}",
                &tr2("Адресов оставлено (нет в копии)", "Links left absolute (not in copy)"),
            )
            .replace("{{t_index}}", &tr2("Полнота индекса", "Index completeness"))
            .replace("{{t_index_total}}", &tr2("Адресов в индексе", "Addresses in the index"))
            .replace("{{t_index_pages}}", &tr2("Страниц индекса обойдено", "Index pages walked"))
            .replace("{{t_index_state}}", &tr2("Состояние индекса", "Index state"))
            .replace(
                "{{t_index_skipped}}",
                &tr2("Пропущено по лимиту", "Skipped by the limit"),
            )
            .replace("{{t_added_live}}", &tr2("Добрано при загрузке", "Added while downloading"))
            .replace("{{t_added_build}}", &tr2("Добрано при сборке", "Added while building"))
            .replace("{{t_sum_check}}", &tr2("Сверка категорий", "Category check"))
            .replace(
                "{{left}}",
                &self
                    .shared
                    .counters
                    .left
                    .load(Ordering::Relaxed)
                    .to_string(),
            )
            .replace("{{index_total}}", &self.index_total.to_string())
            .replace("{{index_pages}}", &self.index_pages.to_string())
            .replace(
                "{{index_state}}",
                &if self.inv_failed {
                    tr2("не получен", "failed")
                } else if self.index_truncated {
                    tr2("обрезан", "truncated")
                } else {
                    tr2("полон", "complete")
                },
            )
            .replace("{{limit_skipped}}", &self.limit_skipped.to_string())
            .replace(
                "{{added_live}}",
                &self.shared.counters.added_live.load(Ordering::Relaxed).to_string(),
            )
            .replace("{{added_build}}", &self.added_build.to_string())
            .replace(
                "{{sum_check}}",
                &{
                    // Сумма категорий обязана сходиться с числом адресов (ФТ-154).
                    let ok = self.shared.counters.ok.load(Ordering::Relaxed);
                    let missing = self.shared.counters.missing.load(Ordering::Relaxed);
                    let errors = self.shared.counters.errors.load(Ordering::Relaxed);
                    let sum = ok + missing + errors;
                    tr2(
                        &format!(
                            "{sum} из {} (скачано {ok}, пропущено {missing}, ошибок {errors})",
                            self.items.len()
                        ),
                        &format!(
                            "{sum} of {} (saved {ok}, missing {missing}, errors {errors})",
                            self.items.len()
                        ),
                    )
                },
            )
            .replace("{{t_snapshot}}", &tr2("Снимки", "Snapshots"))
            .replace("{{t_fresh}}", &tr2("Свежесть копии", "Copy freshness"))
            .replace("{{snap_new}}", &newest)
            .replace("{{snap_old}}", &oldest)
            .replace("{{snap_range}}", &format!("{oldest} — {newest}"))
            .replace("{{old_files}}", &format!("{old_n} ({old_pct:.0}%)"))
            .replace(
                "{{fallbacks}}",
                &self
                    .shared
                    .counters
                    .fallbacks
                    .load(Ordering::Relaxed)
                    .to_string(),
            )
            .replace(
                "{{no_snapshot}}",
                &self
                    .shared
                    .counters
                    .no_snapshot
                    .load(Ordering::Relaxed)
                    .to_string(),
            )
            .replace(
                "{{left}}",
                &self.shared.counters.left.load(Ordering::Relaxed).to_string(),
            )
            .replace(
                "{{shared}}",
                &self
                    .shared
                    .counters
                    .shared
                    .load(Ordering::Relaxed)
                    .to_string(),
            )
            .replace(
                "{{collisions}}",
                &self
                    .shared
                    .counters
                    .collisions
                    .load(Ordering::Relaxed)
                    .to_string(),
            )
            .replace("{{rescued}}", &self.rescued.load(Ordering::Relaxed).to_string())
            .replace("{{fresh_warn}}", &fresh_warn)
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

    /// Записать инвентарь: заголовок с состоянием индекса и строки адресов.
    ///
    /// Вызывается дважды: после сбора списка и повторно после загрузки — вторым
    /// проходом заполняется `same_as` у файлов, полученных ссылкой (ФТ-146).
    fn write_inventory(&self, items: &[Item]) -> Result<(), String> {
        fs::create_dir_all(&self.data_dir).map_err(|e| e.to_string())?;
        // Заголовок инвентаря (ФТ-151): водяной знак и состояние индекса.
        // `read_inventory` такие строки пропускает: в них нет поля `url`.
        let mut inv = format!(
            "{{\"index\":{{\"watermark\":{},\"pages\":{},\"total\":{},\"truncated\":{},\"failed\":{},\"skipped\":{}}}}}\n",
            self.watermark,
            self.index_pages,
            self.index_total,
            self.index_truncated,
            self.inv_failed,
            self.limit_skipped
        );
        let shared = self
            .shared
            .shared_paths
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default();
        for item in items {
            // У файла из инвентаря прежнего прогона поле может быть уже заполнено.
            let same_as = if item.same_as.is_empty() {
                shared.get(&item.path).cloned().unwrap_or_default()
            } else {
                item.same_as.clone()
            };
            inv.push_str(&format!(
                "{{\"url\":\"{}\",\"ts\":\"{}\",\"orig\":\"{}\",\"mime\":\"{}\",\"path\":\"{}\",\"type\":\"{}\",\"len\":{},\"digest\":\"{}\",\"same_as\":\"{}\"}}\n",
                json_escape(&item.url),
                json_escape(&item.ts),
                json_escape(&item.orig),
                json_escape(&item.mime),
                json_escape(&item.path),
                item.kind,
                item.len,
                json_escape(&item.digest),
                json_escape(&same_as)
            ));
        }
        // Атомарная запись: прерванный прогон не оставит половину инвентаря
        // (тот же приём, что для файлов копии: .part + переименование).
        let path = self.data_dir.join("inventory.jsonl");
        let tmp = self.data_dir.join("inventory.jsonl.part");
        fs::write(&tmp, inv).map_err(|e| e.to_string())?;
        if fs::rename(&tmp, &path).is_err() {
            // Windows: rename не перезаписывает существующий файл.
            let _ = fs::remove_file(&path);
            if fs::rename(&tmp, &path).is_err() {
                let _ = fs::copy(&tmp, &path);
                let _ = fs::remove_file(&tmp);
            }
        }
        Ok(())
    }

    pub fn run(&mut self) -> Result<Outcome, String> {
        fs::create_dir_all(&self.out).map_err(|e| e.to_string())?;
        // Временные файлы прошлых прогонов: при прерывании они остаются (Д-16).
        let removed = self.cleanup_temp();
        if removed > 0 {
            self.shared.log(crate::tr::pick(
                format!("уборка: удалено временных файлов {removed}"),
                format!("cleanup: removed {removed} temporary files"),
            ));
        }
        self.load_content_types();
        self.load_snapshots();
        self.inventory()?;
        self.ensure_not_cancelled()?;
        self.check_space()?;
        self.download()?;
        // Инвентарь переписывается после загрузки: только теперь известно, какие
        // адреса получили файл ссылкой (ФТ-146).
        self.write_inventory(&self.items)?;
        self.ensure_not_cancelled()?;
        self.build()?;
        self.save_snapshots();
        self.ensure_not_cancelled()?;
        let mut stats = self.verify();
        self.ensure_not_cancelled()?;
        // Добор по ссылкам: цели, которых нет в копии, догружаются из архива (ФТ-128).
        let broken = self.broken_keys();
        // Если в этом прогоне ничего нового не получено, добор повторил бы тот же
        // неудачный набор запросов: пропускаем и говорим об этом.
        let fresh = self
            .shared
            .counters
            .ok
            .load(Ordering::Relaxed)
            .saturating_sub(self.shared.counters.baseline.load(Ordering::Relaxed));
        let rescued = if fresh == 0 && !broken.is_empty() {
            self.shared.log(crate::tr::pick(
                format!("rescue:skip — копия не менялась, битых {}", broken.len()),
                format!("rescue:skip — nothing new, broken {}", broken.len()),
            ));
            0
        } else {
            self.rescue(&broken)
        };
        if rescued > 0 {
            self.rescued.store(rescued, Ordering::Relaxed);
            self.shared.log(crate::tr::pick(
                format!("rescue:ok:{rescued} — добрано по ссылкам, пересобираю"),
                format!("rescue:ok:{rescued} — rescued by links, rebuilding"),
            ));
            *self.mirror_cache.lock().unwrap() = None;
            self.mirror_files.store(0, Ordering::Relaxed);
            self.build()?;
            self.ensure_not_cancelled()?;
            stats = self.verify();
            self.ensure_not_cancelled()?;
        }
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
            fallbacks: self.shared.counters.fallbacks.load(Ordering::Relaxed),
            collisions: self.shared.counters.collisions.load(Ordering::Relaxed),
            rescued: self.rescued.load(Ordering::Relaxed),
            shared: self.shared.counters.shared.load(Ordering::Relaxed),
            left: self.shared.counters.left.load(Ordering::Relaxed),
            index_total: self.index_total,
            index_pages: self.index_pages,
            index_truncated: self.index_truncated,
            index_failed: self.inv_failed,
            added_live: self.shared.counters.added_live.load(Ordering::Relaxed),
            added_build: 0,
            oldest: self.snapshot_range().0,
            newest: self.snapshot_range().1,
        })
    }

    /// Проверка свободного места до загрузки (Д-15). Оценка — по длине записей индекса
    /// с запасом: распаковка и вторая копия дерева.
    fn check_space(&self) -> Result<(), String> {
        let Some(free) = self.free_space() else {
            return Ok(());
        };
        let need = self.estimate_bytes();
        if need == 0 || need <= free {
            return Ok(());
        }
        let message = crate::tr::pick(
            format!(
                "недостаточно места: нужно около {} МБ, свободно {} МБ. Укажите другую папку или освободите место",
                need / 1_048_576,
                free / 1_048_576
            ),
            format!(
                "not enough space: about {} MB needed, {} MB free. Choose another folder or free some space",
                need / 1_048_576,
                free / 1_048_576
            ),
        );
        self.shared.log(message.clone());
        Err(message)
    }

    /// Ключи целей ссылок, которых нет в копии (для добора по ссылкам).
    fn broken_keys(&self) -> Vec<String> {
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
        let targets = self.ref_targets.lock().unwrap();
        let mut out: Vec<String> = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        for keys in targets.iter() {
            for key in keys.iter() {
                if known.contains(key) || known.contains(&format!("{key}/index.html")) {
                    continue;
                }
                // Ключи с хэшем запроса и чужие хосты не восстанавливаются по адресу.
                if key.contains("__q_")
                    || key.contains('/') && key.split('/').next() == Some(&self.host)
                {
                    continue;
                }
                if seen.insert(key.as_str()) {
                    out.push(key.clone());
                }
            }
        }
        out
    }

    /// Добор по ссылкам (ФТ-128): отсутствующие цели догружаются снимком по их адресу.
    /// Один проход, не больше `rescue_limit` адресов.
    fn rescue(&mut self, missing: &[String]) -> usize {
        if self.rescue_limit == 0 || missing.is_empty() {
            return 0;
        }
        let mut done = 0usize;
        let started = Instant::now();
        for key in missing.iter().take(self.rescue_limit) {
            if self.cancel_token().load(Ordering::Relaxed) {
                break;
            }
            // Бюджет времени: на сайте с тысячами битых ссылок добор иначе занимает
            // минуты, а пользы почти не приносит (ФТ-128).
            if self.rescue_budget > 0 && started.elapsed().as_secs() >= self.rescue_budget {
                self.shared.log(crate::tr::pick(
                    format!(
                        "rescue: бюджет {} с исчерпан, добрано {done} из {}",
                        self.rescue_budget,
                        missing.len().min(self.rescue_limit)
                    ),
                    format!(
                        "rescue: budget {} s spent, rescued {done} of {}",
                        self.rescue_budget,
                        missing.len().min(self.rescue_limit)
                    ),
                ));
                break;
            }
            let url = format!("https://{}/{}", self.host, key.trim_start_matches('/'));
            let item = Item {
                kind: paths::file_type(&url),
                ts: String::new(),
                orig: url.clone(),
                mime: String::new(),
                path: key.clone(),
                url,
                len: 0,
                digest: String::new(),
                same_as: String::new(),
            };
            if self.fetch_target(&item) {
                done += 1;
            }
        }
        if !missing.is_empty() && done == 0 && console_verbose() {
            self.shared.log(crate::tr::pick(
                format!("rescue:fail:{}", missing.len().min(self.rescue_limit)),
                format!("rescue:fail:{}", missing.len().min(self.rescue_limit)),
            ));
        }
        done
    }

    /// Тихое получение снимка по адресу: без счётчиков прогресса, только в зеркало.
    /// Дата полученного снимка записывается так же, как при обычной загрузке (ФТ-129).
    fn fetch_target(&self, item: &Item) -> bool {
        // Добор делает одну попытку по свежему снимку: цепочка из шести запросов
        // на каждую из сотен целей — это тысячи запросов к архиву (ФТ-128).
        let target = SnapshotMode::Newest.target(&item.ts);
        match self.shared.attempt(item, &target, self.stub_bytes) {
            Attempt::Saved { size: _, memento } => {
                self.remember_snapshot(item, memento.unwrap_or(target));
                true
            }
            _ => false,
        }
    }

    /// Запомнить дату снимка файла для журнала и отчёта (ФТ-129).
    fn remember_snapshot(&self, item: &Item, stamp: String) {
        if let Ok(mut map) = self.shared.snapshots.lock() {
            map.insert(item.path.clone(), stamp);
        }
    }
}

/// Подробные сообщения о кодировках — только в CLI (в GUI журнал короткий).
fn console_verbose() -> bool {
    std::env::var("SR_QUIET").is_err()
}

/// Какую версию оставить, когда два адреса дают один путь копии (ФТ-131):
/// свежий снимок важнее; при равенстве — адрес каталога, затем более короткий адрес.
fn keep_previous(candidate: &Item, current: &Item) -> bool {
    if candidate.ts != current.ts {
        return candidate.ts < current.ts;
    }
    let candidate_dir = candidate.url.ends_with('/');
    let current_dir = current.url.ends_with('/');
    if candidate_dir != current_dir {
        return current_dir;
    }
    candidate.url.len() >= current.url.len()
}

/// Свободное место на диске для каталога: платформенный вызов без внешних зависимостей
/// (Д-15). Возвращает `None`, если каталога ещё нет или система не ответила.
#[cfg(windows)]
fn free_bytes(path: &Path) -> Option<u64> {
    if !path.exists() {
        return None;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(
            directory: *const u16,
            free_available: *mut u64,
            total: *mut u64,
            free_total: *mut u64,
        ) -> i32;
    }
    let mut wide: Vec<u16> = path.to_string_lossy().encode_utf16().collect();
    wide.push(0);
    let mut free_available = 0u64;
    // SAFETY: строка с завершающим нулём, все указатели ведут на локальные переменные.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(free_available)
}

/// То же для Linux и прочих unix: `statvfs` даёт свободные блоки и размер блока.
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)]
fn free_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    if !path.exists() {
        return None;
    }
    let c_path = CString::new(path.to_string_lossy().as_bytes()).ok()?;
    // SAFETY: структура целиком заполняется вызовом statvfs, путь корректен.
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: указатель на корректный C-путь и на локальную структуру.
    let ok = unsafe { libc::statvfs(c_path.as_ptr(), &mut stats) };
    // c_ulong на 32-битном Linux уже u32, на 64-битном — u64: приведение нужно в обоих случаях.
    (ok == 0).then(|| (stats.f_bavail as u64).saturating_mul(stats.f_frsize as u64))
}

#[cfg(not(any(windows, unix)))]
fn free_bytes(path: &Path) -> Option<u64> {
    let _ = path;
    None
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

/// Метка времени «сейчас» в формате архива: ГГГГММДДччммсс по UTC.
fn stamp_now() -> String {
    let secs = now_secs();
    stamp_from_secs(secs)
}

/// Метка времени из секунд Unix в формате архива.
fn stamp_from_secs(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}{month:02}{day:02}{:02}{:02}{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Гражданская дата по числу дней от 1970-01-01 (алгоритм Говарда Хиннанта).
fn civil_from_days(days: i64) -> (i64, u64, u64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Разбор `Memento-Datetime` архива: `Tue, 10 Nov 2026 17:07:55 GMT`.
/// Понимает и формат ISO (`2026-11-10T17:07:55Z`) — он встречается у зеркал.
fn stamp_from_http_date(text: &str) -> Option<String> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let value = text.trim();
    // ISO: 2026-11-10T17:07:55Z
    if value.len() >= 19 && value.as_bytes().get(4) == Some(&b'-') {
        let digits: String = value
            .chars()
            .filter(|c| c.is_ascii_digit())
            .take(14)
            .collect();
        return (digits.len() == 14).then_some(digits);
    }
    let mut parts = value.split_whitespace();
    let _weekday = parts.next()?;
    let day: u32 = parts.next()?.trim_end_matches(',').parse().ok()?;
    let month_name = parts.next()?.to_ascii_lowercase();
    let month = MONTHS.iter().position(|m| *m == month_name)? as u32 + 1;
    let year: i64 = parts.next()?.parse().ok()?;
    let time = parts.next()?;
    let mut clock = time.split(':');
    let hour: u32 = clock.next()?.parse().ok()?;
    let minute: u32 = clock.next()?.parse().ok()?;
    let second: u32 = clock.next()?.parse().ok()?;
    Some(format!(
        "{year:04}{month:02}{day:02}{hour:02}{minute:02}{second:02}"
    ))
}

/// Дата снимка в виде `ГГГГ-ММ-ДД` для отчёта и журнала.
fn stamp_readable(ts: &str) -> String {
    if ts.len() < 8 {
        return ts.to_string();
    }
    format!("{}-{}-{}", &ts[..4], &ts[4..6], &ts[6..8])
}

/// Сколько дней прошло с даты снимка до сейчас.
fn stamp_age_days(ts: &str) -> Option<u32> {
    let year: i64 = ts.get(..4)?.parse().ok()?;
    let month: i64 = ts.get(4..6)?.parse().ok()?;
    let day: i64 = ts.get(6..8)?.parse().ok()?;
    let stamp_secs = days_from_civil(year, month as u64, day as u64) as u64 * 86_400;
    Some(((now_secs().saturating_sub(stamp_secs)) / 86_400) as u32)
}

/// Дни от 1970-01-01 для гражданской даты (обратный к `civil_from_days`).
fn days_from_civil(year: i64, month: u64, day: u64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
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
                len: json_number(line, "len").unwrap_or(0),
                url,
                path,
                digest: json_field(line, "digest").unwrap_or_default(),
                same_as: json_field(line, "same_as").unwrap_or_default(),
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

/// Похоже ли содержимое на страницу: есть начало разметки в первых килобайтах.
fn looks_like_markup(body: &[u8]) -> bool {
    let head = &body[..body.len().min(2048)];
    let text = String::from_utf8_lossy(head).to_ascii_lowercase();
    text.contains("<html")
        || text.contains("<!doctype")
        || text.contains("<head")
        || text.contains("<body")
        || text.contains("<title")
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
const JOURNAL_FORMAT: u32 = 2;

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
            targets: Arc::new(targets),
            page,
            foreign,
            snapshot: json_field(line, "snapshot").unwrap_or_default(),
            checked: json_number(line, "checked").unwrap_or(0),
        },
    ))
}

/// Обход дерева: список файлов. Тип записи берётся из самого каталога
/// (`DirEntry::file_type`), без отдельного `stat` на каждый файл — на Windows это
/// снимает два системных вызова с каждой записи и ускоряет обход на порядок.
fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut seen: HashSet<PathBuf> = HashSet::new();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                if seen.insert(path.clone()) {
                    stack.push(path);
                }
            } else if kind.is_file() && !is_temp_file(&path) {
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
<h2>{{t_fresh}}</h2><table>
<tr><td>{{t_snap_range}}</td><td class="n">{{snap_range}}</td></tr>
<tr><td>{{t_old}}</td><td class="n">{{old_files}}</td></tr>
<tr><td>{{t_fallbacks}}</td><td class="n">{{fallbacks}}</td></tr>
<tr><td>{{t_no_snapshot}}</td><td class="n">{{no_snapshot}}</td></tr>
<tr><td>{{t_collisions}}</td><td class="n">{{collisions}}</td></tr>
<tr><td>{{t_rescued}}</td><td class="n">{{rescued}}</td></tr>
<tr><td>{{t_shared}}</td><td class="n">{{shared}}</td></tr>
<tr><td>{{t_left}}</td><td class="n">{{left}}</td></tr></table>

<h2>{{t_index}}</h2>
<table>
<tr><td>{{t_index_total}}</td><td class="n">{{index_total}}</td></tr>
<tr><td>{{t_index_pages}}</td><td class="n">{{index_pages}}</td></tr>
<tr><td>{{t_index_state}}</td><td>{{index_state}}</td></tr>
<tr><td>{{t_index_skipped}}</td><td class="n">{{limit_skipped}}</td></tr>
<tr><td>{{t_added_live}}</td><td class="n">{{added_live}}</td></tr>
<tr><td>{{t_added_build}}</td><td class="n">{{added_build}}</td></tr>
<tr><td>{{t_sum_check}}</td><td>{{sum_check}}</td></tr>
</table>
{{fresh_warn}}
<h2>{{t_integrity}}</h2><table>
<tr><td>{{t_mirror}}</td><td class="n">{{mirror_files}}</td></tr>
<tr><td>{{t_unique}}</td><td class="n">{{unique_paths}}</td></tr>
<tr><td>{{t_foreign}}</td><td class="n">{{foreign}}</td></tr>
<tr><td>{{t_refs}}</td><td class="n">{{refs}}</td></tr>
<tr><td>{{t_broken}} <span style="color:#6b7280">{{t_broken_note}}</span></td><td class="n">{{broken}}</td></tr>
{{encodings}}
<tr><td>{{t_404}}</td><td class="n">{{missing}}</td></tr>
<tr><td>{{t_errors}}</td><td class="n">{{errors}}</td></tr></table>
<p style="color:#b45309;margin-top:10px">{{fresh_warn}}</p>
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

    /// Д-55: очередь не теряет адрес, добавленный во время работы, и не берёт
    /// повторный путь. Закрытие освобождает ожидающего воркера.
    #[test]
    fn queue_takes_live_additions_and_dedupes_paths() {
        let item = |path: &str| Item {
            url: format!("https://example.com/{path}"),
            ts: "20260101000000".to_string(),
            orig: format!("https://example.com/{path}"),
            mime: "text/html".to_string(),
            path: path.to_string(),
            kind: "html",
            len: 1,
            digest: String::new(),
            same_as: String::new(),
        };
        let queue = Arc::new(Queue::new(vec![item("a.html")], 0));
        assert_eq!(queue.planned(), 1);
        assert!(queue.push(item("b.html")), "новый адрес принят");
        assert!(
            !queue.push(item("b.html")),
            "повтор по тому же пути отброшен"
        );
        assert_eq!(queue.planned(), 2, "знаменатель прогресса вырос");

        let taken = queue.pop().map(|i| i.path);
        assert_eq!(taken.as_deref(), Some("a.html"));
        assert_eq!(queue.pop().map(|i| i.path).as_deref(), Some("b.html"));

        // Пустая очередь закрывается: pop возвращает None, а не висит.
        queue.close();
        assert!(queue.pop().is_none());
    }

    /// Д-51: файл с тем же содержимым берётся ссылкой на канонический.
    /// Проверяем содержимое, карту `same_as` и замену уже существующего файла.
    #[test]
    fn link_shared_creates_second_name_with_same_bytes() {
        let dir = std::env::temp_dir().join(format!(
            "sr-link-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::create_dir_all(dir.join("mirror"));
        let engine = Engine::new("example.com", &dir, 1, 0).unwrap();
        let canon = "css/main.css";
        let full = dir.join("mirror").join(canon);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(&full, b"body{color:#000}").unwrap();

        let item = |path: &str| Item {
            url: format!("https://example.com/{path}"),
            ts: "20260101000000".to_string(),
            orig: format!("https://example.com/{path}"),
            mime: "text/css".to_string(),
            path: path.to_string(),
            kind: "css",
            len: 16,
            digest: "TESTDIGEST".to_string(),
            same_as: String::new(),
        };
        let copy = item("css/main_copy.css");
        let size = engine
            .shared
            .link_shared(&copy, canon)
            .expect("ссылка поставлена");
        assert_eq!(size, 16, "размер взят у готового файла");
        let target = dir.join("mirror").join(&copy.path);
        assert_eq!(
            fs::read(&target).unwrap(),
            b"body{color:#000}",
            "содержимое совпадает с каноническим"
        );
        let mapped = engine
            .shared
            .shared_paths
            .lock()
            .unwrap()
            .get(&copy.path)
            .cloned();
        assert_eq!(mapped.as_deref(), Some(canon), "в инвентаре будет same_as");

        // Имя уже занято посторонним файлом: ссылка заменяет его.
        // Важно: писать в существующую жёсткую ссылку нельзя — изменится и
        // канонический файл, поэтому сначала снимаем имя, потом кладём свой файл.
        fs::remove_file(&target).unwrap();
        fs::write(&target, b"stale").unwrap();
        assert!(engine.shared.link_shared(&copy, canon).is_some());
        assert_eq!(fs::read(&target).unwrap(), b"body{color:#000}");
        assert_eq!(
            fs::read(&full).unwrap(),
            b"body{color:#000}",
            "канонический файл не испорчен"
        );

        // Канонический путь сам себе ссылкой не становится.
        assert!(engine.shared.link_shared(&copy, &copy.path).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Д-50: тело пишется в файл потоком, а в памяти остаются только первые
    /// `HEAD_BYTES` байт. Проверяется на настоящем HTTP-ответе от локального
    /// слушателя: 10 000 байт тела больше головы, значит полное чтение в память
    /// не выполнялось, а `.part` существует до проверок пригодности.
    #[test]
    fn save_to_streams_body_and_keeps_only_head() {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let body: Vec<u8> = (0..10_000u32).map(|i| b'a' + (i % 26) as u8).collect();
        let payload = body.clone();
        let server = std::thread::spawn(move || {
            if let Ok((mut socket, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n",
                    payload.len()
                );
                let _ = socket.write_all(head.as_bytes());
                let _ = socket.write_all(&payload);
            }
        });
        let dir = std::env::temp_dir().join(format!(
            "sr-stream-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::create_dir_all(&dir);
        let engine = Engine::new("example.com", &dir, 1, 0).unwrap();
        let dest = dir.join("big.bin");
        // save_to живёт в Shared: именно он владеет агентами и счётчиками.
        let (head, body) = engine
            .shared
            .open(false, &format!("http://{addr}/big"))
            .unwrap();
        let saved = engine.shared.save_body(head, body, &dest).unwrap();
        server.join().unwrap();
        assert_eq!(saved.len, 10_000, "тело должно быть на диске целиком");
        assert_eq!(
            saved.head_bytes.len(),
            HEAD_BYTES,
            "в памяти держится только голова, а не всё тело"
        );
        assert!(saved.tmp.exists(), "до проверок файл лежит в .part");
        assert_eq!(fs::metadata(&saved.tmp).unwrap().len(), 10_000);
        let _ = fs::remove_dir_all(&dir);
    }

    /// 2.4: свёртка строки в карту сохраняет самый поздний снимок и не даёт
    /// добору из полной истории перебить страницу, взятую из окна по дате.
    #[test]
    fn fold_row_keeps_newest_and_spares_window_pages() {
        let mut best: HashMap<String, (String, String, String, u64, String)> = HashMap::new();
        let row = |ts: &str, url: &str, mime: &str| -> Vec<String> {
            vec![
                ts.to_string(),
                url.to_string(),
                mime.to_string(),
                "200".to_string(),
                "10".to_string(),
            ]
        };
        // Старый снимок, затем новый того же адреса.
        fold_row(
            &mut best,
            "example.com",
            &row("20200101000000", "https://example.com/a", "text/html"),
            true,
        );
        fold_row(
            &mut best,
            "example.com",
            &row("20240101000000", "https://example.com/a", "text/html"),
            true,
        );
        assert_eq!(best.len(), 1, "один адрес — одна запись");
        assert_eq!(best["https://example.com/a"].0, "20240101000000");
        // Более старый снимок не должен перебить уже найденный свежий.
        fold_row(
            &mut best,
            "example.com",
            &row("20190101000000", "https://example.com/a", "text/html"),
            true,
        );
        assert_eq!(best["https://example.com/a"].0, "20240101000000");
        // Добор из полной истории не трогает страницу, взятую из окна.
        fold_row(
            &mut best,
            "example.com",
            &row("20250101000000", "https://example.com/a", "text/html"),
            false,
        );
        assert_eq!(
            best["https://example.com/a"].0, "20240101000000",
            "страница из окна не заменяется добором"
        );
        // А ресурс — заменяется: для картинок окно не действует.
        fold_row(
            &mut best,
            "example.com",
            &row("20200101000000", "https://example.com/i.png", "image/png"),
            true,
        );
        fold_row(
            &mut best,
            "example.com",
            &row("20250101000000", "https://example.com/i.png", "image/png"),
            false,
        );
        assert_eq!(best["https://example.com/i.png"].0, "20250101000000");
        // Короткая строка игнорируется, а не паникует.
        fold_row(&mut best, "example.com", &["только".to_string()], true);
        assert_eq!(best.len(), 2);
    }

    /// ФТ-149: смена режима сопоставления меняет сам запрос к CDX.
    /// Проверяется отдельно, потому что до правки запрос собирался один раз
    /// до цикла повторов и переключение на prefix на повтор не влияло.
    #[test]
    fn match_type_changes_query() {
        let dir = std::env::temp_dir().join(format!(
            "sr-matchtype-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::create_dir_all(&dir);
        let engine = Engine::new("example.com", &dir, 1, 0).unwrap();
        assert!(
            engine.quick_query(None).contains("matchType=domain"),
            "по умолчанию режим domain"
        );
        engine.switch_match_type("тест");
        let after = engine.quick_query(None);
        assert!(
            after.contains("matchType=prefix"),
            "после 504 запрос должен идти в режиме prefix: {after}"
        );
        assert_eq!(engine.match_type_now(), MatchType::Prefix);
        let _ = fs::remove_dir_all(&dir);
    }

    /// ФТ-151: заголовочная строка инвентаря с водяным знаком не мешает чтению
    /// адресов — `read_inventory` пропускает записи без поля `url`.
    #[test]
    fn inventory_header_is_skipped_by_reader() {
        let dir = std::env::temp_dir().join(format!(
            "sr-invhead-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("inventory.jsonl");
        let text = concat!(
            "{\"index\":{\"watermark\":1762900000,\"pages\":3,\"total\":1638,",
            "\"truncated\":true,\"failed\":false,\"skipped\":0}}\n",
            "{\"url\":\"https://example.com/\",\"ts\":\"20260101000000\",",
            "\"orig\":\"https://example.com/\",\"mime\":\"text/html\",",
            "\"path\":\"index.html\",\"type\":\"page\",\"len\":10}\n"
        );
        fs::write(&file, text).unwrap();
        let items = read_inventory(&file);
        assert_eq!(items.len(), 1, "заголовок не должен попадать в адреса");
        assert_eq!(items[0].url, "https://example.com/");
        let _ = fs::remove_dir_all(&dir);
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
        let broken = text.replacen("#journal 2", "#journal 1", 1);
        fs::write(&path, broken).unwrap();
        let engine = Engine::new("example.com", &root, 1, 0).unwrap();
        engine.build().expect("сборка без журнала проходит");
        let after = fs::read_to_string(&path).unwrap();
        assert!(after.starts_with(&format!("#journal 2 {}", env!("CARGO_PKG_VERSION"))));
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
        assert_eq!(*entry.targets, vec!["img/x.png", "parts/b/index.html"]);
        assert_eq!(entry.snapshot, "", "старая запись без даты снимка читается");
    }

    /// ФТ-129: дата снимка из журнала переживает запись и чтение.
    #[test]
    fn journal_keeps_snapshot_date() {
        let line = concat!(
            "{\"path\":\"index.html\",\"size\":42,\"mtime\":1794361782,\"page\":true,",
            "\"foreign\":false,\"snapshot\":\"20251115050209\",\"targets\":[]}"
        );
        let (_path, entry) = parse_journal_line(line).expect("строка с датой снимка");
        assert_eq!(entry.snapshot, "20251115050209");
        assert_eq!(stamp_readable(&entry.snapshot), "2025-11-15");
    }

    /// ФТ-126: режим снимка разбирается из командной строки и попадает в метку журнала.
    #[test]
    fn snapshot_mode_parses_and_marks_journal() {
        assert_eq!(SnapshotMode::parse("newest"), Some(SnapshotMode::Newest));
        assert_eq!(SnapshotMode::parse("OLDEST"), Some(SnapshotMode::Oldest));
        assert_eq!(
            SnapshotMode::parse("2023-03-01"),
            Some(SnapshotMode::At("20230301000000".to_string()))
        );
        assert_eq!(
            SnapshotMode::parse("2024"),
            Some(SnapshotMode::At("20240000000000".to_string()))
        );
        assert_eq!(SnapshotMode::parse("позавчера"), None);
        assert_eq!(SnapshotMode::Newest.tag(), "newest");
        assert_eq!(
            SnapshotMode::At("20230301000000".to_string()).tag(),
            "at:20230301000000"
        );
    }

    /// ФТ-129: `Memento-Datetime` архива разбирается в отпечаток, включая вариант ISO.
    #[test]
    fn memento_datetime_is_parsed() {
        assert_eq!(
            stamp_from_http_date("Tue, 10 Nov 2026 17:07:55 GMT"),
            Some("20261110170755".to_string())
        );
        assert_eq!(
            stamp_from_http_date("Mon, 09 Nov 2026 13:03:57 GMT"),
            Some("20260911130357".to_string().replace("20260911", "20261109"))
        );
        assert_eq!(
            stamp_from_http_date("2026-11-10T17:07:55Z"),
            Some("20261110170755".to_string())
        );
        assert_eq!(stamp_from_http_date("не дата"), None);
    }

    /// Календарные преобразования: обратный переход совпадает с прямым.
    #[test]
    fn civil_dates_round_trip() {
        for (year, month, day) in [(1970, 1, 1), (2015, 11, 1), (2026, 11, 10), (2024, 2, 29)] {
            let days = days_from_civil(year, month, day);
            assert_eq!(civil_from_days(days), (year, month, day));
        }
        // 2026-11-10T17:07:55Z
        let stamp = stamp_from_secs(1_794_330_475);
        assert!(
            stamp.starts_with("2026"),
            "метка времени в 2026 году: {stamp}"
        );
        assert_eq!(stamp.len(), 14);
    }

    /// ФТ-131: при совпадении пути остаётся свежий снимок, при равенстве — адрес каталога.
    #[test]
    fn keep_previous_prefers_fresh_snapshot() {
        let item = |url: &str, ts: &str| Item {
            url: url.to_string(),
            ts: ts.to_string(),
            orig: url.to_string(),
            mime: "text/html".to_string(),
            path: "index.html".to_string(),
            kind: "html",
            len: 0,
            digest: String::new(),
            same_as: String::new(),
        };
        let fresh = item("https://example.com/index.html", "20260101000000");
        let old = item("https://example.com/", "20150101000000");
        assert!(!keep_previous(&fresh, &old), "свежий вытесняет старый");
        assert!(keep_previous(&old, &fresh), "старый не вытесняет свежий");

        let dir = item("https://example.com/", "20260101000000");
        let page = item("https://example.com/index.html", "20260101000000");
        assert!(
            !keep_previous(&dir, &page),
            "при равенстве остаётся адрес каталога"
        );
        assert!(keep_previous(&page, &dir));
    }

    /// ФТ-127: пригодность проверяется по разметке и порогу пустой страницы.
    #[test]
    fn markup_detection_distinguishes_stub() {
        assert!(looks_like_markup(b"<!DOCTYPE html><html><body>x"));
        assert!(looks_like_markup(b"<html><head><title>x</title>"));
        assert!(!looks_like_markup(b"Not Found"));
        assert!(!looks_like_markup(b""));
    }

    /// Д-20: пауза задерживает новые запросы, снятие паузы их отпускает.
    #[test]
    fn pause_blocks_new_requests() {
        let root = workdir("pause");
        let engine = Engine::new("example.com", &root, 1, 0).unwrap();
        let flag = engine.pause_token();
        flag.store(true, Ordering::Relaxed);
        let shared = Arc::clone(&engine.shared);
        let started = Instant::now();
        // Признак «воркер вошёл в ожидание»: без него на загруженной машине поток
        // мог стартовать уже после снятия паузы, и тест падал ложно (наблюдалось).
        let ready = Arc::new(AtomicBool::new(false));
        let go = Arc::clone(&ready);
        let worker = std::thread::spawn(move || {
            go.store(true, Ordering::Relaxed);
            shared.wait_for_pause();
            started.elapsed()
        });
        let spin = Instant::now();
        while !ready.load(Ordering::Relaxed) && spin.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(1));
        }
        std::thread::sleep(Duration::from_millis(120));
        flag.store(false, Ordering::Relaxed);
        let waited = worker.join().unwrap();
        assert!(
            waited >= Duration::from_millis(100),
            "пауза должна задерживать запрос, прошло {waited:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Д-25: ограничитель конкурентности держит лимит и отпускает всех при его смене.
    #[test]
    fn gate_respects_limit() {
        let gate = Arc::new(Gate::new(1));
        let inflight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..4 {
            let gate = Arc::clone(&gate);
            let inflight = Arc::clone(&inflight);
            let peak = Arc::clone(&peak);
            workers.push(std::thread::spawn(move || {
                gate.acquire();
                let now = inflight.fetch_add(1, Ordering::Relaxed) + 1;
                peak.fetch_max(now, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(20));
                inflight.fetch_sub(1, Ordering::Relaxed);
                gate.release();
            }));
        }
        std::thread::sleep(Duration::from_millis(40));
        gate.set_limit(3);
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(gate.limit(), 3, "лимит меняется на ходу");
        assert!(peak.load(Ordering::Relaxed) <= 3, "лимит не превышается");
    }

    /// Д-25: счётчики — атомарные и складываются без потерь.
    #[test]
    fn counters_add_up_under_threads() {
        let counters = Arc::new(Counters::default());
        let mut workers = Vec::new();
        for _ in 0..8 {
            let counters = Arc::clone(&counters);
            workers.push(std::thread::spawn(move || {
                for _ in 0..500 {
                    counters.done.fetch_add(1, Ordering::Relaxed);
                    counters.ok.fetch_add(1, Ordering::Relaxed);
                    counters.bytes.fetch_add(10, Ordering::Relaxed);
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(counters.done.load(Ordering::Relaxed), 4000);
        assert_eq!(counters.ok.load(Ordering::Relaxed), 4000);
        assert_eq!(counters.bytes.load(Ordering::Relaxed), 40_000);
    }

    /// ФТ-130: файл с устаревшим снимком попадает в перекачку, свежий — нет.
    #[test]
    fn stale_paths_uses_snapshot_age() {
        let root = workdir("stale");
        put(&root, "old.html", b"<html><body>old</body></html>");
        put(&root, "new.html", b"<html><body>new</body></html>");
        let engine = build(&root);
        // Подменяем время проверки в журнале: один файл проверен давно, другой сегодня.
        let path = root.join("data").join("build.jsonl");
        let text = fs::read_to_string(&path).unwrap();
        let mut out = String::new();
        for line in text.lines() {
            if !line.starts_with('{') {
                out.push_str(line);
                out.push('\n');
                continue;
            }
            let checked = if line.contains("old.html") {
                0
            } else {
                now_secs()
            };
            let patched = match line.find("\"checked\":") {
                Some(at) => {
                    let head = &line[..at + "\"checked\":".len()];
                    let tail: String = line[at + "\"checked\":".len()..]
                        .chars()
                        .skip_while(|c| c.is_ascii_digit())
                        .collect();
                    format!("{head}{checked}{tail}")
                }
                None => line.to_string(),
            };
            out.push_str(&patched);
            out.push('\n');
        }
        fs::write(&path, out).unwrap();

        let mut second = Engine::new("example.com", &root, 1, 0).unwrap();
        second.set_snapshot(SnapshotMode::Newest);
        second.set_refresh_days(30);
        let stale = second.stale_paths();
        assert!(stale.contains("old.html"), "старый снимок перекачивается");
        assert!(!stale.contains("new.html"), "свежий снимок не трогаем");

        // В режиме oldest журнал ведёт себя как в 3.4.0: перекачки нет.
        second.set_snapshot(SnapshotMode::Oldest);
        assert!(second.stale_paths().is_empty());
        let _ = engine;
        let _ = fs::remove_dir_all(&root);
    }

    /// Д-26: числа отчёта совпадают с содержимым копии.
    #[test]
    fn report_numbers_match_copy() {
        // Язык подписей в отчёте — глобальный на процесс: тест про английский
        // отчёт переключает его под замком, поэтому здесь тот же замок. Без него
        // отчёт изредка собирался по-английски и проверка русской подписи падала
        // (плавающий отказ, ловился 1 раз на 10–15 прогонов).
        let _lang = LANG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::tr::set_english(false);
        let root = workdir("reportnb");
        put(
            &root,
            "index.html",
            b"<html><body><a href=\"/a.html\">a</a><img src=\"/i.png\"><a href=\"/gone.html\">g</a></body></html>",
        );
        put(
            &root,
            "a.html",
            b"<html><body><a href=\"/index.html\">home</a></body></html>",
        );
        put(
            &root,
            "i.png",
            b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0",
        );
        let engine = build(&root);
        let stats = engine.verify();
        engine.report(stats).unwrap();

        // Пересчёт по диску: файлов в копии столько же, сколько лежит в папке site.
        let files = walk_files(&root.join("site"));
        let doc = fs::read_to_string(root.join("report.html")).unwrap();
        assert_eq!(stats.pages, 2, "две страницы");
        assert_eq!(stats.broken, 1, "одна цель отсутствует");
        assert!(
            doc.contains(&format!("<td class='n'>{}</td>", files.len()))
                || doc.contains(&format!("<div class=\"v\">{}</div>", files.len())),
            "число файлов копии есть в отчёте ({})",
            files.len()
        );
        assert!(
            doc.contains("Свежесть копии"),
            "в отчёте есть раздел свежести"
        );
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
