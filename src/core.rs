//! Ядро: инвентаризация архива, параллельная загрузка с AIMD, сборка оффлайн-копии,
//! проверка ссылок и отчёт.

use crate::paths;
use crate::sanitize::sanitize_site;

use rayon::prelude::*;
use regex::Regex;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Идентификация клиента в архиве (Д-98): версия берётся из `Cargo.toml`, а не
/// пишется руками — иначе она отстаёт от изделия, и по журналам архива нельзя
/// понять, какой сборкой сделан запрос. Согласована с `--version` (`APP_VERSION`).
pub const UA: &str = concat!(
    "SiteRestorer/",
    env!("CARGO_PKG_VERSION"),
    " (offline mirror tool; respectful crawler; contact: local user)"
);
const MIN_THREADS: usize = 2;
const MAX_THREADS: usize = 24;
const PAGE_LIMIT: usize = 20_000;
/// Основной адрес архива: сначала шифрованный канал, открытый — запасной.
/// Часть сетей блокирует порт 80, часть — только 443 (ОШ-2 в docs/DEFECTS.md).
const ARCHIVE_PRIMARY: &str = "https://web.archive.org";
const ARCHIVE_FALLBACK: &str = "http://web.archive.org";
/// Сколько попыток даётся на обращение к индексу архива.
const ARCHIVE_TRIES: u32 = 5;
/// Сколько неудач на запасной схеме архива терпим, прежде чем вернуться на https
/// (уточнено 14.11.2026: две неудачи подряд не должны закрывать прогон).
const ARCHIVE_ALT_TRIES: usize = 3;
/// Предел паузы по `Retry-After`: сервер может прислать `inf` или огромное число,
/// и неограниченная пауза вешала прогон (а `inf` ронял его паникой).
const MAX_RETRY_WAIT: f64 = 300.0;
/// Сколько потоков загрузки берётся, если пользователь не задал своё число.
pub const DEFAULT_WORKERS: usize = 16;
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
/// Потолок тела ответа индекса (Р3-1): защищает от неограниченного раздутия Vec/gzip.
const MAX_INDEX_BYTES: usize = 512 * 1024 * 1024;
/// Порог размера текстового файла для переписывания ссылок по умолчанию (ФТ-142).
const DEFAULT_TEXT_LIMIT: usize = 4 * 1024 * 1024;
/// Порог свободного места для аварийной остановки во время загрузки (Д-101).
const MIN_FREE_DURING_RUN: u64 = 16 * 1024 * 1024;

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

/// Разбор даты снимка/окна с проверкой формата и календарных границ (Р3-6).
pub(crate) fn parse_stamp_input(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '-' | '/' | ':' | ' ' | 'T' | 't' | 'Z' | 'z'))
    {
        return None;
    }
    let digits: String = trimmed.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() < 4 || digits.len() > 14 || !digits.len().is_multiple_of(2) {
        return None;
    }
    if digits.len() >= 6 {
        let month: u32 = digits[4..6].parse().ok()?;
        if !(1..=12).contains(&month) {
            return None;
        }
    }
    if digits.len() >= 8 {
        let day: u32 = digits[6..8].parse().ok()?;
        if !(1..=31).contains(&day) {
            return None;
        }
    }
    if digits.len() >= 10 {
        let hour: u32 = digits[8..10].parse().ok()?;
        if hour > 23 {
            return None;
        }
    }
    if digits.len() >= 12 {
        let minute: u32 = digits[10..12].parse().ok()?;
        if minute > 59 {
            return None;
        }
    }
    if digits.len() == 14 {
        let sec: u32 = digits[12..14].parse().ok()?;
        if sec > 60 {
            return None;
        }
    }
    let mut stamp = digits;
    while stamp.len() < 14 {
        stamp.push('0');
    }
    Some(stamp)
}

impl SnapshotMode {
    /// Разбор значения командной строки. Дата принимается как 4, 6, 8, 10, 12 или 14 цифр.
    pub fn parse(text: &str) -> Option<Self> {
        let value = text.trim().to_ascii_lowercase();
        match value.as_str() {
            "newest" | "latest" | "fresh" => return Some(Self::Newest),
            "oldest" | "first" => return Some(Self::Oldest),
            _ => {}
        }
        parse_stamp_input(text).map(Self::At)
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
    /// Ответ пришёл со `Content-Encoding: gzip` — распаковывать его или нет,
    /// решает вызывающий (Д-121): индекс разжимаем, файлы копии — никогда.
    pub gzip: bool,
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

#[derive(Debug)]
enum SaveErr {
    Cancelled,
    Stream(String),
    Io(String),
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
    /// Сколько каталогов копии и зеркала не удалось прочитать (Д-107).
    pub unreadable: usize,
    /// Схема доступа к архиву, с которой прогон закончил: `https` или `http` (Д-97).
    pub archive: String,
    /// Сколько запросов к архиву сделано за прогон (Д-97).
    pub archive_requests: usize,
}

pub type Cb = Arc<dyn Fn(&Progress) + Send + Sync>;

// ---------------------------------------------------------------- gate
struct GateState {
    limit: usize,
    cap: usize,
    inflight: usize,
}

/// Динамический ограничитель конкурентности: лимит меняется на ходу (AIMD),
/// но не превышает заданный пользователем потолок `cap`.
struct Gate {
    state: Mutex<GateState>,
    cv: Condvar,
}

impl Gate {
    #[cfg(test)]
    fn new(limit: usize) -> Self {
        Self::new_capped(limit, MAX_THREADS)
    }

    fn new_capped(limit: usize, cap: usize) -> Self {
        let cap = cap.clamp(1, MAX_THREADS);
        Self {
            state: Mutex::new(GateState {
                limit: limit.clamp(1, cap),
                cap,
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

    fn cap(&self) -> usize {
        self.state.lock().unwrap().cap
    }

    fn set_limit(&self, n: usize) {
        let mut st = self.state.lock().unwrap();
        st.limit = n.clamp(1, st.cap);
        self.cv.notify_all();
    }
}

/// Защищённый слот шлюза `Gate`: позволяет освобождать конкурентный слот на
/// время паузы ожидания повтора и автоматически возвращает слот при выходе.
struct GateSlot<'a> {
    gate: &'a Gate,
    held: bool,
}

impl<'a> GateSlot<'a> {
    fn acquire(gate: &'a Gate) -> Self {
        gate.acquire();
        Self { gate, held: true }
    }

    fn suspend(&mut self) {
        if self.held {
            self.gate.release();
            self.held = false;
        }
    }

    fn resume(&mut self) {
        if !self.held {
            self.gate.acquire();
            self.held = true;
        }
    }
}

impl Drop for GateSlot<'_> {
    fn drop(&mut self) {
        if self.held {
            self.gate.release();
        }
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

    /// Пометить пути (например, уже лежащие на диске) как известные очереди (Р3-4).
    fn mark_seen<'a>(&self, paths: impl IntoIterator<Item = &'a str>) {
        if let Ok(mut seen) = self.seen.lock() {
            for p in paths {
                seen.insert(p.to_string());
            }
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
        // Флаг ставится под тем же мьютексом, на котором спят воркеры: иначе
        // закрытие попадает в окно между проверкой `closed` и `cv.wait`, сигнал
        // теряется и `join` в конце прогона висит навсегда.
        let _guard = self.items.lock().unwrap_or_else(|e| e.into_inner());
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
    /// Сколько из них оставлено внутри скриптов (.js и <script>) (ФТ-147).
    script_left: AtomicUsize,
    /// Сколько каталогов не удалось прочитать при обходе дерева (Д-107).
    unreadable: AtomicUsize,
    /// Сколько сжатых gzip-файлов распаковано при сохранении (Д-138).
    gunzipped: AtomicUsize,
    /// Сколько gzip-файлов не удалось распаковать (битое тело).
    gunzip_failed: AtomicUsize,
    /// Ресурсов с HTML внутри вместо ожидаемого стиля/скрипта/картинки (Д-140).
    mismatched: AtomicUsize,
    bytes: AtomicU64,
    win_bytes: AtomicU64,
    win_files: AtomicUsize,
    win_429: AtomicUsize,
    /// Перегрузки сервера без 429: 5xx, таймауты, отказы соединения (10061) в загрузке.
    win_overload: AtomicUsize,
    /// Суммарная длительность (мс) и число ответов в текущем окне для оценки задержки.
    win_req_ms: AtomicU64,
    win_req_cnt: AtomicUsize,
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
    /// Неудачи на запасной схеме подряд: после `ARCHIVE_ALT_TRIES` возвращаемся на https.
    alt_failures: AtomicUsize,
    /// Возврат на https уже был (второй раз не возвращаемся, чтобы не ходить по кругу).
    archive_back: AtomicBool,
    /// Сколько запросов к архиву сделано за прогон (Д-97): видно в RESULT и отчёте.
    archive_requests: AtomicUsize,
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
    /// Причина остановки из-за нехватки места во время загрузки (Д-101).
    disk_full_error: Mutex<Option<String>>,
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

    /// Переключает схему доступа к архиву после сетевой ошибки (ОШ-2).
    ///
    /// Переключение «липкое» (ФТ-116, Д-97): схема, выбранная при первой неудаче,
    /// держится до конца прогона. Обратное переключение (`fetch_xor`) возвращало
    /// https после первого же удачного запроса, и прогон ходил по кругу: каждый
    /// третий запрос снова падал по таймауту. Новый прогон начинает с https.
    /// Сообщение в журнал — один раз на прогон, чтобы не засорять его.
    fn switch_archive(&self, reason: &str) {
        let first = self
            .archive_switched
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok();
        if first && !self.archive_switch_logged.swap(true, Ordering::Relaxed) {
            let to = self.archive_base().to_string();
            self.log(crate::tr::pick(
                format!("архив: {reason}, переключаюсь на {to}"),
                format!("archive: {reason}, switching to {to}"),
            ));
        }
    }

    /// Ошибка архива: менять ли схему (уточнено 14.11.2026 на живом прогоне).
    ///
    /// Переключение на запасную схему оправдано только тогда, когда адрес
    /// недоступен целиком: соединение не устанавливается, DNS не отвечает, TLS не
    /// согласуется. Медленный ответ живого сервера — не повод: https работал, просто
    /// долго, а порт 80 архива часто закрыт, и после «сдачи» схема до конца прогона
    /// оставалась нерабочей (прогон падал на инвентаризации).
    ///
    /// Если запасная схема тоже не отвечает несколько раз, программа возвращается
    /// на https: две неудачи подряд не должны закрывать прогон.
    fn archive_failed(&self, reason: &str, err: &str) {
        if !scheme_switch_worthwhile(err) {
            return;
        }
        if self.archive_switched.load(Ordering::Relaxed) {
            let failures = self.alt_failures.fetch_add(1, Ordering::Relaxed) + 1;
            if failures >= ARCHIVE_ALT_TRIES && !self.archive_back.swap(true, Ordering::Relaxed) {
                self.archive_switched.store(false, Ordering::Relaxed);
                let to = self.archive_base().to_string();
                self.log(crate::tr::pick(
                    format!("архив: {reason} и на запасной схеме — возвращаюсь на {to}"),
                    format!("archive: {reason} on the fallback too — back to {to}"),
                ));
            }
            return;
        }
        self.switch_archive(reason);
    }

    /// Действующая схема доступа к архиву: `https` или `http` (Д-97).
    fn archive_scheme(&self) -> &'static str {
        if self.archive_switched.load(Ordering::Relaxed) {
            "http"
        } else {
            "https"
        }
    }

    /// Сколько запросов к архиву сделано за прогон (Д-97).
    fn archive_requests(&self) -> usize {
        self.archive_requests.load(Ordering::Relaxed)
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
        self.open_with_cancel(index, url, &self.cancel)
    }

    fn open_with_cancel(
        &self,
        index: bool,
        url: &str,
        extra_cancel: &AtomicBool,
    ) -> Result<(Head, ureq::Body), String> {
        if self.cancel.load(Ordering::Relaxed) || extra_cancel.load(Ordering::Relaxed) {
            return Err("cancelled".to_string());
        }
        // Счётчик запросов к архиву (Д-97): по нему в итоге видно, сколько раз
        // программа ходила за снимками, — иначе «долго» и «много запросов» не
        // различить по журналу.
        self.archive_requests.fetch_add(1, Ordering::Relaxed);
        self.wait_if_paused();
        self.rate_wait();
        if self.cancel.load(Ordering::Relaxed) || extra_cancel.load(Ordering::Relaxed) {
            return Err("cancelled".to_string());
        }
        let agent = if index {
            &self.agent_index
        } else {
            &self.agent
        };
        // Файл копии принимается без сжатия (Д-108): архив иногда помечает тело
        // `Content-Encoding: gzip`, хотя внутри не gzip, и файл терялся с ошибкой
        // «invalid gzip header». В зеркало должны попадать байты, которые отдал
        // архив, а не результат распаковки. Индекс — наоборот: JSON жмётся полезно.
        let encoding = if index { "gzip" } else { "identity" };
        let t0 = Instant::now();
        let resp = agent
            .get(url)
            .header("User-Agent", UA)
            .header("Accept", "*/*")
            .header("Accept-Encoding", encoding)
            .call()
            .map_err(|e| e.to_string())?;
        if !index {
            let ms = t0.elapsed().as_millis() as u64;
            self.counters.win_req_ms.fetch_add(ms, Ordering::Relaxed);
            self.counters.win_req_cnt.fetch_add(1, Ordering::Relaxed);
        }
        let head = Head {
            status: resp.status().as_u16(),
            retry_after: resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(retry_after_secs),
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
            gzip: resp
                .headers()
                .get("content-encoding")
                .and_then(|v| v.to_str().ok())
                .map(|v| v.to_ascii_lowercase().contains("gzip"))
                .unwrap_or(false),
        };
        Ok((head, resp.into_body()))
    }

    /// Прочитать тело целиком. Между блоками проверяется отмена (Д-23):
    /// остановка не ждёт таймаута запроса, соединение закрывается сразу.
    /// Побочно снимается потолок 10 МиБ, который был у `read_to_vec` в ureq 3.
    fn read_body_limited(
        &self,
        body: ureq::Body,
        gzip: bool,
        tolerant_error: bool,
        extra_cancel: &AtomicBool,
        mut on_progress: Option<&mut dyn FnMut(u64, usize)>,
    ) -> Result<Vec<u8>, String> {
        let mut out: Vec<u8> = Vec::new();
        let mut lines = 0usize;
        let mut last = Instant::now();
        let raw = body.into_reader();
        let mut reader: Box<dyn std::io::Read + Send> = if gzip {
            Box::new(flate2::read::GzDecoder::new(raw))
        } else {
            Box::new(raw)
        };
        let mut chunk = [0u8; 64 * 1024];
        loop {
            if self.cancel.load(Ordering::Relaxed) || extra_cancel.load(Ordering::Relaxed) {
                return Err("cancelled".to_string());
            }
            let read = match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => n,
                // При ответе 429/503/504 с `Content-Encoding: gzip` и пустым/HTML телом
                // декодер падает до возврата статуса; статус и Retry-After важнее тела (Р3-1).
                Err(_) if tolerant_error => return Ok(Vec::new()),
                Err(err) => return Err(err.to_string()),
            };
            if out.len().saturating_add(read) > MAX_INDEX_BYTES {
                return Err(crate::tr::t(
                    "ответ архива слишком велик (> 512 МБ)",
                    "archive response exceeds 512 MB limit",
                )
                .to_string());
            }
            let part = &chunk[..read];
            out.extend_from_slice(part);
            lines += part.iter().filter(|b| **b == b'\n').count();
            if let Some(cb) = on_progress.as_deref_mut()
                && last.elapsed() >= Duration::from_millis(200)
            {
                cb(out.len() as u64, lines);
                last = Instant::now();
            }
        }
        if let Some(cb) = on_progress {
            cb(out.len() as u64, lines);
        }
        Ok(out)
    }

    fn fetch(&self, url: &str) -> Result<Fetched, String> {
        let (head, body) = self.open(false, url)?;
        let err_status = head.status != 200;
        Ok(Fetched {
            status: head.status,
            body: self.read_body_limited(
                body,
                head.gzip && !err_status,
                err_status,
                &self.cancel,
                None,
            )?,
            retry_after: head.retry_after,
        })
    }

    /// То же, что `fetch`, но с отчётом о прогрессе чтения (индекс).
    fn fetch_stream(
        &self,
        url: &str,
        on_progress: impl FnMut(u64, usize),
    ) -> Result<Fetched, String> {
        self.fetch_stream_until(url, &self.cancel, on_progress)
    }

    fn fetch_stream_until(
        &self,
        url: &str,
        extra_cancel: &AtomicBool,
        mut on_progress: impl FnMut(u64, usize),
    ) -> Result<Fetched, String> {
        let (head, body) = self.open_with_cancel(true, url, extra_cancel)?;
        let err_status = head.status != 200;
        Ok(Fetched {
            status: head.status,
            body: self.read_body_limited(
                body,
                head.gzip && !err_status,
                err_status,
                extra_cancel,
                Some(&mut on_progress),
            )?,
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
        if !seconds.is_finite() {
            return;
        }
        let end = Instant::now() + Duration::from_secs_f64(seconds.clamp(0.0, MAX_RETRY_WAIT));
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
        fs::metadata(long_path(&from))
            .ok()
            .filter(|m| m.len() > 0)?;
        if let Some(parent) = to.parent() {
            let _ = fs::create_dir_all(long_path(parent));
        }
        // Занятое имя мешает и ссылке, и переименованию: убираем старый файл.
        let _ = fs::remove_file(long_path(&to));
        if fs::hard_link(long_path(&from), long_path(&to)).is_err()
            && fs::copy(long_path(&from), long_path(&to)).is_err()
        {
            return None;
        }
        let size = fs::metadata(long_path(&to)).ok()?.len() as usize;
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
        if let Ok(meta) = fs::metadata(long_path(&full))
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
            Attempt::Saved { memento } => {
                self.counters.ok.fetch_add(1, Ordering::Relaxed);
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
        let mut slot = GateSlot::acquire(&self.gate);
        for attempt in 0..2u32 {
            if self.cancel.load(Ordering::Relaxed) {
                return Attempt::Failed;
            }
            if self.paused.load(Ordering::Relaxed) {
                slot.suspend();
                self.wait_for_pause();
                slot.resume();
            }
            // Один запрос на файл: заголовки решают, что делать с телом, и оно же
            // пишется на диск. Раньше здесь сначала шёл `fetch` (тело в память),
            // а затем `save_to` качал тот же файл вторым запросом — трафик ×2.
            let (head, body) = match self.open(false, &url) {
                Ok(pair) => pair,
                Err(err) => {
                    if err == "cancelled" {
                        return Attempt::Failed;
                    }
                    transport_failed = true;
                    self.counters.win_overload.fetch_add(1, Ordering::Relaxed);
                    let reason = explain_net(&err);
                    self.log(crate::tr::pick(
                        format!("сеть: {reason}"),
                        format!("network: {reason}"),
                    ));
                    // Другая схема — только при отказе соединения, не при медленном ответе.
                    self.archive_failed("сеть недоступна", &err.to_string());
                    if attempt == 0 {
                        slot.suspend();
                        self.sleep(0.6);
                        slot.resume();
                    }
                    continue;
                }
            };
            let status = head.status;
            if status == 429 {
                drain_short(body);
                self.counters.win_429.fetch_add(1, Ordering::Relaxed);
                // Ограничение частоты — состояние транспорта. Раньше после двух
                // 429 адрес помечался как «снимка нет» и выпадал из прогонов на
                // неделю (TTL), хотя снимок в архиве есть.
                transport_failed = true;
                let wait = head.retry_after.unwrap_or(20.0 * (attempt + 1) as f64);
                self.log(crate::tr::pick(
                    format!("429: пауза {wait:.0} с"),
                    format!("429: pause {wait:.0}s"),
                ));
                if attempt == 0 {
                    slot.suspend();
                    self.sleep(wait);
                    slot.resume();
                }
                continue;
            }
            if status == 404 {
                drain_short(body);
                return Attempt::Absent;
            }
            if status == 200 {
                // 200 — тело того же ответа пишется в файл потоком.
                match self.save_body(head, body, &self.mirror.join(&item.path)) {
                    Ok(saved) => {
                        return match self.commit(item, &saved, stub) {
                            Some(done) => done,
                            None => Attempt::Unusable,
                        };
                    }
                    Err(SaveErr::Cancelled) => return Attempt::Failed,
                    Err(SaveErr::Io(err)) => {
                        self.log(crate::tr::pick(
                            format!("запись: {err}"),
                            format!("write: {err}"),
                        ));
                        return Attempt::Failed;
                    }
                    Err(SaveErr::Stream(err)) => {
                        transport_failed = true;
                        self.counters.win_overload.fetch_add(1, Ordering::Relaxed);
                        let reason = explain_net(&err);
                        self.log(crate::tr::pick(
                            format!("сеть (поток): {reason}"),
                            format!("network (stream): {reason}"),
                        ));
                        if attempt == 0 {
                            slot.suspend();
                            self.sleep(0.6);
                            slot.resume();
                        }
                        continue;
                    }
                }
            }
            drain_short(body);
            // 3xx без пригодного содержимого и прочие коды: содержимого нет.
            if (300..400).contains(&status) {
                return Attempt::Unusable;
            }
            transport_failed = true;
            if status >= 500 {
                self.counters.win_overload.fetch_add(1, Ordering::Relaxed);
            }
            if attempt == 0 {
                slot.suspend();
                self.sleep(head.retry_after.unwrap_or(0.8));
                slot.resume();
            }
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
    fn save_body(&self, head: Head, body: ureq::Body, dest: &Path) -> Result<Saved, SaveErr> {
        if let Some(parent) = dest.parent() {
            let _ = fs::create_dir_all(long_path(parent));
        }
        let tmp = tmp_path(dest);
        let mut file =
            fs::File::create(long_path(&tmp)).map_err(|e| SaveErr::Io(explain_io_error(&e)))?;
        let mut reader = body.into_reader();
        let mut chunk = [0u8; 64 * 1024];
        let mut len: u64 = 0;
        let mut head_bytes: Vec<u8> = Vec::new();
        let cleanup = |tmp: &Path| {
            let _ = fs::remove_file(long_path(tmp));
        };
        loop {
            if self.cancel.load(Ordering::Relaxed) {
                drop(file);
                cleanup(&tmp);
                return Err(SaveErr::Cancelled);
            }
            let read = match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => n,
                Err(err) => {
                    drop(file);
                    cleanup(&tmp);
                    return Err(SaveErr::Stream(err.to_string()));
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
                return Err(SaveErr::Io(explain_io_error(&err)));
            }
            self.counters
                .bytes
                .fetch_add(read as u64, Ordering::Relaxed);
            self.counters
                .win_bytes
                .fetch_add(read as u64, Ordering::Relaxed);
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
        let mut unpacked_head: Option<Vec<u8>> = None;
        let mut actual_len = saved.len;
        // П-1 / Д-138: архив нередко отдаёт валидный gzip без `Content-Encoding`.
        // Распаковываем текстовые файлы копии (CSS/JS/SVG/HTML/...) по магии `1f 8b`.
        if saved.head_bytes.starts_with(&[0x1F, 0x8B])
            && is_gzip_text_ext(&item.path)
            && saved.len <= 16 * 1024 * 1024
            && let Ok(raw) = fs::read(long_path(&saved.tmp))
        {
            match gunzip_bytes_limited(&raw, 64 * 1024 * 1024) {
                Some(plain) => {
                    if fs::write(long_path(&saved.tmp), &plain).is_ok() {
                        actual_len = plain.len() as u64;
                        let take = plain.len().min(HEAD_BYTES);
                        unpacked_head = Some(plain[..take].to_vec());
                        self.counters.gunzipped.fetch_add(1, Ordering::Relaxed);
                    }
                }
                None => {
                    self.counters.gunzip_failed.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        let head = unpacked_head.as_deref().unwrap_or(&saved.head_bytes);
        let html = item.kind == "html";
        // П-4 / Д-140: если под видом .css/.js/.ico/.svg/.png отдаётся HTML-заглушка,
        // снимок непригоден — позволяем fallback подобрать настоящий ресурс.
        let html_ct = saved
            .head
            .content_type
            .as_deref()
            .is_some_and(|ct| ct.to_ascii_lowercase().contains("text/html"));
        if !html
            && is_non_html_resource(&item.path)
            && (html_ct || is_html_doc_head(head))
            && is_html_doc_head(head)
        {
            self.counters.mismatched.fetch_add(1, Ordering::Relaxed);
            let _ = fs::remove_file(long_path(&saved.tmp));
            return None;
        }
        // «Пустая страница» проверяется по полному размеру, остальное — по голове.
        let small_page = html && (actual_len < stub.max(20) as u64 || !looks_like_markup(head));
        let usable = actual_len > 0 && !is_error_page(head) && !small_page;
        if !usable {
            let _ = fs::remove_file(long_path(&saved.tmp));
            return None;
        }
        let full = self.mirror.join(&item.path);
        if fs::rename(long_path(&saved.tmp), long_path(&full)).is_err() {
            // Windows заменяет файл при переименовании (`MoveFileEx` с
            // `MOVEFILE_REPLACE_EXISTING`), и отказ означает не «файл уже есть», а
            // занятый файл: его держит антивирус, индексатор или сама программа
            // (Д-103 — прежний комментарий объяснял это неверно). Поэтому пробуем
            // ещё раз после удаления, а копирование оставлено страховкой для чужих
            // файловых систем.
            let _ = fs::remove_file(long_path(&full));
            if fs::rename(long_path(&saved.tmp), long_path(&full)).is_err() {
                let _ = fs::copy(long_path(&saved.tmp), long_path(&full));
                let _ = fs::remove_file(long_path(&saved.tmp));
            }
        }
        // Файл мог не пережить переименование: успех считаем только по факту.
        let _size = fs::metadata(long_path(&full))
            .ok()
            .filter(|m| m.len() > 0)?
            .len();
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
            memento: saved.head.memento.clone(),
        })
    }

    /// Последние снимки адреса по индексу, от свежего к старому (для добора).
    fn recent_stamps(&self, item: &Item, depth: usize) -> Vec<String> {
        let _slot = GateSlot::acquire(&self.gate);
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
            .iter()
            .filter_map(|row| row.field(&fetched.body, 0).map(Cow::into_owned))
            .collect();
        stamps.sort_by(|a, b| b.cmp(a));
        stamps
    }
}

/// Извлечь пути файлов из query-параметра минификатора `?f=a.css,b.css` (П-9, Д-143).
fn minifier_param_paths(raw: &str) -> Vec<String> {
    let Some((_, query)) = raw.split_once('?') else {
        return Vec::new();
    };
    let query = query.split('#').next().unwrap_or(query);
    let mut out = Vec::new();
    for pair in query.split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        if k == "f" || k == "files" {
            let dec = crate::sanitize::pct_decode(v);
            for part in dec.split(',') {
                let clean = part.trim().trim_start_matches('/');
                if !clean.is_empty() && !clean.contains("..") {
                    out.push(clean.to_string());
                }
            }
        }
    }
    out
}

/// Текстовое расширение, которое при магии `1f 8b` надо распаковывать (П-1, Д-138).
fn is_gzip_text_ext(path: &str) -> bool {
    let low = path.to_ascii_lowercase();
    [
        ".html", ".htm", ".css", ".js", ".mjs", ".svg", ".xml", ".json", ".txt",
    ]
    .iter()
    .any(|ext| low.ends_with(ext))
}

/// Ресурсы, в которых не должно лежать HTML-содержимое (П-4, Д-140).
fn is_non_html_resource(path: &str) -> bool {
    let low = path.to_ascii_lowercase();
    [
        ".css", ".js", ".mjs", ".svg", ".ico", ".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp",
        ".woff", ".woff2", ".ttf", ".otf", ".eot", ".pdf",
    ]
    .iter()
    .any(|ext| low.ends_with(ext))
}

/// Проверяет, начинается ли тело с реального HTML-документа (не SVG/XML-фрагмент).
fn is_html_doc_head(head: &[u8]) -> bool {
    let text = String::from_utf8_lossy(&head[..head.len().min(1024)]).to_ascii_lowercase();
    let t = text.trim_start();
    t.starts_with("<!doctype html")
        || t.starts_with("<html")
        || t.contains("<html") && t.contains("<head")
}

/// Распаковать gzip-тело с потолком размера; возвращает `None` при ошибке формата или превышении.
fn gunzip_bytes_limited(raw: &[u8], max_out: usize) -> Option<Vec<u8>> {
    if !raw.starts_with(&[0x1F, 0x8B]) {
        return None;
    }
    let mut dec = flate2::read::GzDecoder::new(raw);
    let mut out = Vec::with_capacity(raw.len().saturating_mul(3).min(max_out));
    let mut chunk = [0u8; 32 * 1024];
    loop {
        match dec.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if out.len().saturating_add(n) > max_out {
                    return None;
                }
                out.extend_from_slice(&chunk[..n]);
            }
            Err(_) => return None,
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Дочитывает короткое тело ошибочных/пустых ответов (до 64 КБ), чтобы в
/// HTTP/1.1 соединение оставалось чистым и возвращалось в пул `ureq` (Д-134).
fn drain_short(body: ureq::Body) {
    let mut reader = body.into_reader();
    let mut buf = [0u8; 8 * 1024];
    let mut left = 64 * 1024usize;
    while left > 0 {
        let want = buf.len().min(left);
        match reader.read(&mut buf[..want]) {
            Ok(0) | Err(_) => break,
            Ok(n) => left -= n,
        }
    }
}

/// Свернуть одну строку CDX в карту «адрес → самый поздний снимок» (2.4).
///
/// Строка: `[отпечаток, исходный адрес, тип, код, длина]`. Если адрес уже есть,
/// остаётся снимок с большим отпечатком. `from_window = false` означает добор из
/// полной истории: тогда страницы не трогаются — они уже взяты из окна (ФТ-128).
/// Совпадает ли содержимое файла с записанным в журнале (ФТ-146).
///
/// Если отпечатка нет ни там, ни там (архив не отдал `digest`), считаем, что
/// содержимое то же: иначе на каждом прогоне пересобиралась бы вся копия.
fn prev_digest_ok(prev: Option<&JournalEntry>, current: &str) -> bool {
    match prev {
        Some(entry) => entry.digest == current,
        None => false,
    }
}

fn fold_row(
    best: &mut HashMap<String, (String, String, String, u64, String)>,
    host: &str,
    row: &Row,
    body: &[u8],
    from_window: bool,
) {
    if row.len() < 3 {
        return;
    }
    let Some(orig) = row.field(body, 1) else {
        return;
    };
    let url = paths::normalize_url(&orig, host);
    if !from_window && paths::file_type(&url) == "html" && best.contains_key(&url) {
        return;
    }
    // Значения остаются срезами до момента вставки: строка, проигравшая по дате,
    // не порождает копий ts, mime и отпечатка. Адрес нормализуется до сравнения
    // (это нужно для поиска в карте) и там аллокация неизбежна.
    let ts = row.field(body, 0).unwrap_or_default();
    let mime = row.field(body, 2).unwrap_or_default();
    let len = row
        .field(body, 4)
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    // Шестое поле запроса — отпечаток содержимого (ФТ-145); последняя строка
    // может прийти короче, тогда отпечатка просто нет.
    let digest = row.field(body, 5).unwrap_or_default();
    match best.get(&url) {
        Some((old_ts, _, _, _, _)) if old_ts.as_str() >= ts.as_ref() => {}
        _ => {
            best.insert(
                url,
                (
                    ts.into_owned(),
                    orig.into_owned(),
                    mime.into_owned(),
                    len,
                    digest.into_owned(),
                ),
            );
        }
    }
}

/// Временный файл прошлого прогона: `.part` или `.jsonl.part`.
fn is_temp_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(".part") || n.ends_with(".jsonl.part"))
}

/// Удаляет все `.part` / `.jsonl.part` файлы в каталоге рекурсивно с `long_path`.
fn clean_temp_dir(root: &Path) -> usize {
    let mut removed = 0usize;
    let mut stack = vec![root.to_path_buf()];
    let mut seen: HashSet<PathBuf> = HashSet::new();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(long_path(&dir)) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            let path = entry.path();
            if kind.is_dir() {
                if seen.insert(path.clone()) {
                    stack.push(path);
                }
            } else if kind.is_file()
                && is_temp_file(&path)
                && fs::remove_file(long_path(&path)).is_ok()
            {
                removed += 1;
            }
        }
    }
    removed
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
    /// Сколько страниц индекса ожидалось по оценке `showNumPages` (Д-136).
    index_expected_pages: usize,
    /// Сколько страниц индекса реально обойдено (ФТ-149).
    index_pages: usize,
    /// Дополнительные хосты/поддомены, включаемые в копию (Д-142).
    include_hosts: Vec<String>,
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
    /// Порог текстового файла для переписывания ссылок (ФТ-142, 0 — без предела).
    pub text_limit: usize,
    /// Сколько текстовых файлов скопировано без переписывания из-за предела (ФТ-143).
    text_skipped: AtomicUsize,
    /// Логировано примеров превышения --text-limit.
    text_skipped_logged: AtomicUsize,
    /// Сколько примеров оставленных абсолютных адресов в скриптах залогировано (ФТ-147).
    script_left_logged: AtomicUsize,
    /// Накопленный счётчик оставленных в скриптах абсолютных адресов (ФТ-147).
    script_left_total: usize,
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
    /// Накопленные за все сборки прогона счётчики: после добора сборка идёт
    /// второй раз, и без накопления отчёт показывал двойной счёт.
    enc_total: std::sync::Mutex<HashMap<&'static str, usize>>,
    foreign_total: usize,
    left_total: usize,
    enc_logged: std::sync::atomic::AtomicUsize,
    /// Список файлов зеркала: дерево обходится один раз за прогон (было восемь обходов).
    mirror_cache: Mutex<Option<Arc<Vec<PathBuf>>>>,
    /// Деревья, по которым непрочитанные каталоги уже учтены (Д-107).
    unreadable_seen: Mutex<HashSet<PathBuf>>,
    /// Первые пути непрочитанных каталогов с причиной (П-Д4).
    unreadable_paths: Mutex<Vec<String>>,
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
    /// Сжатые файлы зеркала, распакованные на этапе сборки (Д-138).
    build_gunzipped: AtomicUsize,
    /// Битые сжатые файлы, встреченные при сборке.
    build_gunzip_failed: AtomicUsize,
    /// Подмены ресурсов (HTML в .css/.js/.ico/.svg) на этапе сборки (Д-140).
    build_mismatched: AtomicUsize,
    /// Склеенные минификатором файлы (Д-143).
    minifier_built: AtomicUsize,
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
    /// Отпечаток содержимого из индекса (ФТ-145): по нему вместе с размером
    /// решается, нужно ли пересобирать файл, — время изменения для этого
    /// ненадёжно (ФТ-146).
    digest: String,
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
        let cap = workers.clamp(1, MAX_THREADS);
        let file_config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(3)
            .max_idle_connections(64)
            .max_idle_connections_per_host(MAX_THREADS + 8)
            .max_idle_age(Duration::from_secs(60))
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_recv_response(Some(Duration::from_secs(TIMEOUT_FILE)))
            .timeout_per_read(Some(Duration::from_secs(30)))
            .build();
        let index_config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(3)
            .max_idle_connections(8)
            .max_idle_connections_per_host(4)
            .max_idle_age(Duration::from_secs(60))
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
            index_expected_pages: 0,
            index_total: 0,
            index_pages: 0,
            include_hosts: Vec::new(),
            limit_skipped: 0,
            rescue_limit: DEFAULT_RESCUE_LIMIT,
            rescue_budget: DEFAULT_RESCUE_BUDGET,
            retry_missing_days: DEFAULT_RETRY_MISSING_DAYS,
            stub_bytes: DEFAULT_STUB_BYTES,
            text_limit: DEFAULT_TEXT_LIMIT,
            text_skipped: AtomicUsize::new(0),
            text_skipped_logged: AtomicUsize::new(0),
            script_left_logged: AtomicUsize::new(0),
            script_left_total: 0,
            rescued: AtomicUsize::new(0),
            shared: Arc::new(Shared {
                agent: ureq::Agent::new_with_config(file_config),
                agent_index: ureq::Agent::new_with_config(index_config),
                match_type: Mutex::new(MatchType::Domain),
                agent_archive: ARCHIVE_PRIMARY.to_string(),
                archive_alt: ARCHIVE_FALLBACK.to_string(),
                archive_switched: AtomicBool::new(false),
                archive_switch_logged: AtomicBool::new(false),
                alt_failures: AtomicUsize::new(0),
                archive_back: AtomicBool::new(false),
                archive_requests: AtomicUsize::new(0),
                mirror: out.join("mirror"),
                gate: Gate::new_capped(MIN_THREADS.min(cap), cap),
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
                disk_full_error: Mutex::new(None),
            }),
            started: Instant::now(),
            items: Vec::new(),
            inv_failed: false,
            added_build: 0,
            enc_stats: std::sync::Mutex::new(HashMap::new()),
            enc_total: std::sync::Mutex::new(HashMap::new()),
            foreign_total: 0,
            left_total: 0,
            enc_logged: std::sync::atomic::AtomicUsize::new(0),
            mirror_cache: Mutex::new(None),
            unreadable_seen: Mutex::new(HashSet::new()),
            unreadable_paths: Mutex::new(Vec::new()),
            site_cache: Mutex::new(None),
            pages: AtomicUsize::new(0),
            foreign_built: AtomicUsize::new(0),
            site_files: AtomicUsize::new(0),
            mirror_files: AtomicUsize::new(0),
            ref_targets: Mutex::new(Vec::new()),
            journal: Mutex::new(HashMap::new()),
            write_errors: AtomicUsize::new(0),
            build_gunzipped: AtomicUsize::new(0),
            build_gunzip_failed: AtomicUsize::new(0),
            build_mismatched: AtomicUsize::new(0),
            minifier_built: AtomicUsize::new(0),
        })
    }

    /// Учесть непрочитанные каталоги дерева ровно один раз за прогон (Д-107).
    ///
    /// Одно и то же дерево обходится несколько раз (кэш списка зеркала, кэш копии,
    /// пересчёт после сборки). Без этой проверки число в отчёте складывалось бы из
    /// повторных обходов и завышалось.
    fn note_unreadable(&self, root: &Path, details: &[(PathBuf, String)]) {
        if details.is_empty() {
            return;
        }
        let Ok(mut seen) = self.unreadable_seen.lock() else {
            return;
        };
        if seen.insert(root.to_path_buf()) {
            self.shared
                .counters
                .unreadable
                .fetch_add(details.len(), Ordering::Relaxed);
            if let Ok(mut list) = self.unreadable_paths.lock() {
                for (path, err) in details {
                    let line = format!("{} ({err})", show_path(path));
                    if list.len() < 20 {
                        list.push(line.clone());
                    }
                    if list.len() <= 5 {
                        self.shared.log(crate::tr::pick(
                            format!("каталог не прочитан: {line}"),
                            format!("directory unreadable: {line}"),
                        ));
                    }
                }
            }
        }
    }

    /// Список файлов зеркала: один обход на прогон (раньше дерево обходилось восемь раз).
    fn mirror_list(&self) -> Arc<Vec<PathBuf>> {
        let mut cache = self.mirror_cache.lock().unwrap();
        if let Some(list) = cache.as_ref() {
            return Arc::clone(list);
        }
        let (list, unreadable) = walk_files_detailed(&self.mirror);
        self.note_unreadable(&self.mirror.clone(), &unreadable);
        let list = Arc::new(list);
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
        let (list, unreadable) = walk_files_detailed(&self.site_dir);
        self.note_unreadable(&self.site_dir.clone(), &unreadable);
        let list = Arc::new(list);
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
        if fs::create_dir_all(long_path(dir)).is_ok() {
            cache.lock().unwrap().insert(dir.to_path_buf());
        }
    }

    /// Пишет в журнал первые ошибки записи: раньше они терялись, а отчёт
    /// показывал несуществующие файлы как собранные.
    fn log_write_error(&self, rel: &str, err: &std::io::Error) {
        if self.write_errors.fetch_add(1, Ordering::Relaxed) < 10 {
            let reason = explain_io_error(err);
            self.shared.log(crate::tr::pick(
                format!("не удалось записать {rel}: {reason}"),
                format!("cannot write {rel}: {reason}"),
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
        let Ok(text) = fs::read_to_string(long_path(&path)) else {
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
        let _ = fs::create_dir_all(long_path(&self.data_dir));
        let _ = fs::write(long_path(&self.content_types_path()), out);
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

    /// Порог размера текстового файла для переписывания ссылок (ФТ-142, 0 — без предела).
    pub fn set_text_limit(&mut self, bytes: usize) {
        self.text_limit = bytes;
    }

    /// Добавить дополнительный хост/поддомен в копию (`--include-host`, Д-142).
    pub fn set_include_hosts(&mut self, hosts: Vec<String>) {
        self.include_hosts = hosts
            .into_iter()
            .filter_map(|h| sanitize_site(&h).map(|(host, _)| host))
            .collect();
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
        let Ok(text) = fs::read_to_string(long_path(&self.snapshots_path())) else {
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
        let _ = fs::create_dir_all(long_path(&self.data_dir));
        let path = self.snapshots_path();
        let tmp = path.with_extension("tsv.part");
        if fs::write(long_path(&tmp), out).is_ok() {
            let _ = fs::rename(long_path(&tmp), long_path(&path));
        }
    }

    /// Прочитать список адресов без снимка: `url`, табуляция, время проверки.
    fn load_no_snapshot(&self) -> HashMap<String, u64> {
        let Ok(text) = fs::read_to_string(long_path(&self.no_snapshot_path())) else {
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
        let _ = fs::create_dir_all(long_path(&self.data_dir));
        let _ = fs::write(long_path(&self.no_snapshot_path()), out);
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
                if !long_path(&self.mirror.join(path)).exists() {
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

    /// Удалить временные файлы прошлых прогонов (Д-16, ОШ-15).
    ///
    /// `walk_files_detailed` отфильтровывает `.part`, чтобы они не попали в сборку,
    /// поэтому для уборки используется отдельный обход без этого фильтра.
    fn cleanup_temp(&self) -> usize {
        let mut removed = 0usize;
        if long_path(&self.mirror).exists() {
            removed += clean_temp_dir(&self.mirror);
            *self.mirror_cache.lock().unwrap() = None;
        }
        if long_path(&self.data_dir).exists() {
            removed += clean_temp_dir(&self.data_dir);
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
        // Известна ли оценка объёма: без неё пустой ответ нельзя считать
        // доказательством «страниц нет вовсе».
        let mut estimate_ok = false;
        for est_try in 0..3u32 {
            match self
                .shared
                .fetch(&self.cdx_query("showNumPages=true&output=json"))
            {
                Ok(fetched) if fetched.status == 200 => {
                    if let Some(n) = parse_pages(&String::from_utf8_lossy(&fetched.body)) {
                        pages = n;
                        estimate_ok = true;
                        self.index_expected_pages = n;
                    }
                    break;
                }
                Ok(fetched) if (fetched.status == 429 || fetched.status >= 500) && est_try < 2 => {
                    self.shared.log(format!(
                        "CDX: оценка недоступна (HTTP {}), повтор {}/3",
                        fetched.status,
                        est_try + 2
                    ));
                    let wait = fetched.retry_after.unwrap_or(2.0 * (est_try + 1) as f64);
                    self.shared.sleep(wait);
                }
                Ok(fetched) => {
                    self.shared
                        .log(format!("CDX: оценка недоступна (HTTP {})", fetched.status));
                    break;
                }
                Err(err) => {
                    if self.shared.cancel.load(Ordering::Relaxed) {
                        return Err("cancelled".to_string());
                    }
                    self.shared.archive_failed("оценка недоступна", &err);
                    if est_try < 2 {
                        self.shared.sleep(2.0 * (est_try + 1) as f64);
                    } else {
                        let reason = explain_net(&err);
                        self.shared.log(crate::tr::pick(
                            format!("CDX: оценка — {reason}"),
                            format!("CDX: estimate failed: {reason}"),
                        ));
                    }
                }
            }
        }

        // Быстрый запрос выполняется ВСЕГДА: постраничный режим (`page=N`) у
        // web.archive.org поддерживается не для всех сайтов и бывает пустым,
        // а запрос с limit=50000 отдаёт те же данные.
        // Строки индекса свёртываются в карту сразу, а не копятся в Vec:
        // 200 страниц по 20 000 строк давали до 4 млн строк в памяти (2.4).
        let mut best: HashMap<String, (String, String, String, u64, String)> = HashMap::new();
        // Сколько строк разобрано: для проверки усечения и для отчёта.
        let mut parsed_rows = 0usize;
        // Если showNumPages точно говорит, что страниц больше одной, быстрый запрос
        // без &page= может отдать только первый блок (наблюдалось на prokupol.ru:
        // 518 вместо 1639) — идём сразу на постраничный обход 0..pages (П-Д1).
        let force_paging = estimate_ok && pages > 1;
        if !force_paging {
            self.emit(
                "inventory",
                4.0,
                crate::tr::t("получение списка адресов", "fetching the address list"),
                true,
            );
        }
        for attempt in 0..ARCHIVE_TRIES {
            if force_paging {
                break;
            }
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
                        fold_row(&mut best, &self.host, row, &fetched.body, true);
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
                        // Отказ сервера, а не отсутствие страниц: иначе прогон
                        // заканчивался словами «archive has no pages». Признак
                        // снимается, если список всё же получен (ниже).
                        self.inv_failed = true;
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
                    let reason = explain_net(&err);
                    self.shared.log(crate::tr::pick(
                        format!("CDX: список — {reason}"),
                        format!("CDX: list failed: {reason}"),
                    ));
                    self.shared
                        .archive_failed("список недоступен", &err.to_string());
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
        // Пустой ответ при известной оценке (одна страница) и без фильтров по
        // времени означает «в архиве нет страниц»: постраничный обход ничего не
        // найдёт, а стоит минуты ожидания на 4 % (визуальный тест 14.11.2026).
        let empty_for_sure = best.is_empty()
            && estimate_ok
            && pages <= 1
            && self.since.is_none()
            && self.window_to().is_none();
        let mut need_paging = force_paging;
        if force_paging {
            self.shared.log(crate::tr::pick(
                format!("CDX: индекс состоит из {pages} страниц — запускаю постраничный обход"),
                format!("CDX: index has {pages} pages — starting paged crawl"),
            ));
        } else if empty_for_sure {
            self.shared.log(crate::tr::t(
                "CDX: в архиве нет страниц для этого адреса",
                "CDX: the archive has no pages for this address",
            ));
        } else if best.is_empty() {
            self.shared.log(crate::tr::t(
                "CDX: быстрый запрос пуст — постраничный обход",
                "CDX: quick query empty — switching to paging",
            ));
            pages = pages.max(2);
            need_paging = true;
        } else if parsed_rows + QUICK_SLACK >= QUICK_LIMIT {
            // Оказаться у самой границы можно и на полном индексе: тогда обход
            // просто пройдёт целиком и снимет признак усечения (ниже).
            self.index_truncated = true;
            need_paging = true;
            self.shared.log(crate::tr::pick(
                format!("CDX: список обрезан на {QUICK_LIMIT} адресах"),
                format!("CDX: address list truncated at {QUICK_LIMIT}"),
            ));
        } else if !estimate_ok {
            // Если оценка числа страниц не ответила, проверяем page=1, чтобы
            // не принять 1-ю страницу за весь индекс (П-Д1).
            match self
                .shared
                .fetch_stream(&self.paged_query(1), &mut |_, _| {})
            {
                Ok(fetched) if fetched.status == 200 => {
                    let extra = parse_rows(&fetched.body);
                    if extra.is_empty() {
                        self.index_pages = 1;
                    } else {
                        self.shared.log(crate::tr::t(
                            "CDX: без оценки обнаружена следующая страница — запускаю полный обход",
                            "CDX: next page found without estimate — switching to full paging",
                        ));
                        best.clear();
                        pages = pages.max(2);
                        need_paging = true;
                    }
                }
                _ => {
                    self.index_pages = 1;
                    self.index_truncated = true;
                    self.shared.log(crate::tr::t(
                        "CDX: оценка недоступна, вторая страница не ответила — полнота индекса не подтверждена",
                        "CDX: estimate unavailable and page 2 failed — index completeness unverified",
                    ));
                }
            }
        } else {
            // Оценка показала 1 страницу и быстрый запрос уложился в порог.
            self.index_pages = 1;
        }
        let estimate_known = estimate_ok && pages > 1;
        if need_paging {
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
            // Обход закончился на странице короче PAGE_LIMIT — индекс собран целиком.
            let mut finished = false;
            let mut restarted = false;
            if estimate_known && max_pages > 1 {
                // Параллельный обход известных страниц (Д-136): 2 потока при старте,
                // сброс до 1 при 503/429 и до ARCHIVE_TRIES (5) попыток на страницу.
                'restart_par: loop {
                    if self.shared.cancel.load(Ordering::Relaxed) {
                        return Err("cancelled".to_string());
                    }
                    let next_page = AtomicUsize::new(0);
                    let cdx_limit = AtomicUsize::new(2usize.min(max_pages));
                    let cdx_inflight = Mutex::new(0usize);
                    let cdx_cv = Condvar::new();
                    let abort_restart = AtomicBool::new(false);
                    let done_pages = AtomicUsize::new(0);
                    let ok_pages = AtomicUsize::new(0);
                    let failed_any = AtomicBool::new(false);
                    let empty_early = AtomicBool::new(false);
                    let best_lock = Mutex::new(&mut best);

                    let n_workers = 2usize.min(max_pages);
                    std::thread::scope(|s| {
                        for wid in 0..n_workers {
                            let next_page = &next_page;
                            let cdx_limit = &cdx_limit;
                            let cdx_inflight = &cdx_inflight;
                            let cdx_cv = &cdx_cv;
                            let abort_restart = &abort_restart;
                            let done_pages = &done_pages;
                            let ok_pages = &ok_pages;
                            let failed_any = &failed_any;
                            let empty_early = &empty_early;
                            let best_lock = &best_lock;
                            let this = &*self;
                            s.spawn(move || {
                                if wid > 0 {
                                    std::thread::sleep(Duration::from_millis(150));
                                }
                                loop {
                                    if this.shared.cancel.load(Ordering::Relaxed)
                                        || abort_restart.load(Ordering::Relaxed)
                                    {
                                        return;
                                    }
                                    let page_no = next_page.fetch_add(1, Ordering::Relaxed);
                                    if page_no >= max_pages {
                                        return;
                                    }
                                    let query = this.paged_query(page_no);
                                    let mut attempts = 0u32;
                                    loop {
                                        if this.shared.cancel.load(Ordering::Relaxed)
                                            || abort_restart.load(Ordering::Relaxed)
                                        {
                                            return;
                                        }
                                        {
                                            let mut inf = cdx_inflight.lock().unwrap();
                                            while *inf >= cdx_limit.load(Ordering::Relaxed) {
                                                inf = cdx_cv.wait(inf).unwrap();
                                            }
                                            *inf += 1;
                                        }
                                        this.emit(
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
                                                let overall = 4.0
                                                    + 4.0 * (page_no as f32 + inner as f32)
                                                        / shown_pages as f32;
                                                this.emit(
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
                                            this.shared.fetch_stream(&query, &mut on_page)
                                        };
                                        {
                                            let mut inf = cdx_inflight.lock().unwrap();
                                            *inf = inf.saturating_sub(1);
                                            cdx_cv.notify_all();
                                        }
                                        match result {
                                            Ok(fetched) if fetched.status == 200 => {
                                                let chunk = parse_rows(&fetched.body);
                                                if chunk.is_empty() {
                                                    if page_no == 0 && !restarted {
                                                        let before = this.match_type_now();
                                                        this.switch_match_type("пустая первая страница");
                                                        if this.match_type_now() != before {
                                                            abort_restart.store(true, Ordering::Relaxed);
                                                            return;
                                                        }
                                                    }
                                                    empty_early.store(true, Ordering::Relaxed);
                                                    done_pages.fetch_add(1, Ordering::Relaxed);
                                                    break;
                                                }
                                                {
                                                    let mut guard = best_lock.lock().unwrap();
                                                    for row in &chunk {
                                                        fold_row(
                                                            *guard,
                                                            &this.host,
                                                            row,
                                                            &fetched.body,
                                                            true,
                                                        );
                                                    }
                                                }
                                                ok_pages.fetch_add(1, Ordering::Relaxed);
                                                done_pages.fetch_add(1, Ordering::Relaxed);
                                                break;
                                            }
                                            Ok(fetched) => {
                                                let code = fetched.status;
                                                let retry = fetched.retry_after;
                                                if code == 504 && !restarted {
                                                    let before = this.match_type_now();
                                                    this.switch_match_type("HTTP 504 на странице");
                                                    if this.match_type_now() != before {
                                                        abort_restart.store(true, Ordering::Relaxed);
                                                        return;
                                                    }
                                                }
                                                if code == 429 || code >= 500 {
                                                    cdx_limit.store(1, Ordering::Relaxed);
                                                    attempts += 1;
                                                    if attempts >= ARCHIVE_TRIES {
                                                        this.shared.log(format!(
                                                            "CDX: страница {} — HTTP {code} после {attempts} попыток, пропускаю",
                                                            page_no + 1
                                                        ));
                                                        failed_any.store(true, Ordering::Relaxed);
                                                        done_pages.fetch_add(1, Ordering::Relaxed);
                                                        break;
                                                    }
                                                    let wait = retry.unwrap_or(10.0 * attempts as f64).min(30.0);
                                                    this.shared.log(crate::tr::pick(
                                                        format!("CDX: HTTP {code} (стр. {}), пауза {wait:.0} с", page_no + 1),
                                                        format!("CDX: HTTP {code} (page {}), pause {wait:.0}s", page_no + 1),
                                                    ));
                                                    this.shared.sleep(wait);
                                                    continue;
                                                }
                                                this.shared.log(crate::tr::pick(
                                                    format!("CDX: страница {} — HTTP {code}", page_no + 1),
                                                    format!("CDX: page {} — HTTP {code}", page_no + 1),
                                                ));
                                                failed_any.store(true, Ordering::Relaxed);
                                                done_pages.fetch_add(1, Ordering::Relaxed);
                                                break;
                                            }
                                            Err(err) => {
                                                if this.shared.cancel.load(Ordering::Relaxed) {
                                                    return;
                                                }
                                                cdx_limit.store(1, Ordering::Relaxed);
                                                attempts += 1;
                                                this.shared.archive_failed(
                                                    "страница недоступна",
                                                    &err.to_string(),
                                                );
                                                if attempts >= ARCHIVE_TRIES {
                                                    let reason = explain_net(&err);
                                                    this.shared.log(crate::tr::pick(
                                                        format!("CDX: страница {} — {reason}", page_no + 1),
                                                        format!("CDX: page {} failed: {reason}", page_no + 1),
                                                    ));
                                                    failed_any.store(true, Ordering::Relaxed);
                                                    done_pages.fetch_add(1, Ordering::Relaxed);
                                                    break;
                                                }
                                                this.shared.sleep(5.0 * attempts as f64);
                                                continue;
                                            }
                                        }
                                    }
                                }
                            });
                        }
                    });
                    if self.shared.cancel.load(Ordering::Relaxed) {
                        return Err("cancelled".to_string());
                    }
                    if abort_restart.load(Ordering::Relaxed) && !restarted {
                        restarted = true;
                        best.clear();
                        self.index_pages = 0;
                        continue 'restart_par;
                    }
                    self.index_pages = ok_pages.load(Ordering::Relaxed);
                    if empty_early.load(Ordering::Relaxed) {
                        self.index_truncated = true;
                        self.shared.log(crate::tr::t(
                            "CDX: пустая страница — индекс остаётся обрезанным",
                            "CDX: empty page — the index stays truncated",
                        ));
                    }
                    if failed_any.load(Ordering::Relaxed) {
                        self.inv_failed = true;
                    } else if self.index_pages >= pages
                        && max_pages >= pages
                        && !empty_early.load(Ordering::Relaxed)
                    {
                        finished = true;
                    }
                    break 'restart_par;
                }
            } else {
                let mut page = 0usize;
                let mut attempts = 0u32;
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
                            if got == 0 && page == 0 {
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
                            if got == 0 {
                                if !estimate_known {
                                    finished = true;
                                } else {
                                    self.index_truncated = true;
                                    self.shared.log(crate::tr::t(
                                        "CDX: пустая страница — индекс остаётся обрезанным",
                                        "CDX: empty page — the index stays truncated",
                                    ));
                                }
                                break;
                            }
                            for row in &chunk {
                                fold_row(&mut best, &self.host, row, &fetched.body, true);
                            }
                            page += 1;
                            self.index_pages = page;
                            if estimate_known && page >= pages && max_pages >= pages {
                                finished = true;
                                break;
                            }
                            continue;
                        }
                        Ok(fetched) => {
                            let code = fetched.status;
                            let retry = fetched.retry_after;
                            if code == 504 {
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
                                if attempts >= ARCHIVE_TRIES {
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
                            self.shared
                                .archive_failed("страница недоступна", &err.to_string());
                            if attempts >= ARCHIVE_TRIES {
                                let reason = explain_net(&err);
                                self.shared.log(crate::tr::pick(
                                    format!("CDX: страница {} — {reason}", page + 1),
                                    format!("CDX: page {} failed: {reason}", page + 1),
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
                    fold_row(&mut best, &self.host, &row, &fetched.body, false);
                }
            }
        }

        // Дополнительные хосты/поддомены (--include-host или при переходе на prefix, Д-142).
        for inc in self.include_hosts.clone() {
            if inc == self.host {
                continue;
            }
            let extra = format!(
                "output=json&fl=timestamp,original,mimetype,statuscode,length,digest&filter=statuscode:200&collapse=urlkey&limit={QUICK_LIMIT}"
            );
            let q = format!(
                "{}/cdx/search/cdx?url={}&matchType=prefix&{}",
                self.shared.archive_base(),
                urlencode(&inc),
                extra
            );
            for _ in 0..3u32 {
                if let Ok(fetched) = self.shared.fetch_stream(&q, &mut |_, _| {})
                    && fetched.status == 200
                {
                    for row in parse_rows(&fetched.body) {
                        fold_row(&mut best, &self.host, &row, &fetched.body, true);
                    }
                    break;
                }
                self.shared.sleep(2.0);
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
            // Д-103: по `urlkey` первыми идут мусорные адреса (`/%22`, `/%22Cern`),
            // поэтому проба на 20 адресах показывала не сайт, а его обломки. Порядок:
            // главная, затем корневые разделы по числу страниц, затем остальное.
            items = limit_order(items);
            items.truncate(self.limit);
        }
        {
            let mut types = self.shared.types.lock().unwrap();
            for item in &items {
                types.slot(item.kind)[1] += 1;
            }
        }
        fs::create_dir_all(long_path(&self.data_dir)).map_err(|e| explain_io_error(&e))?;
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
        fs::create_dir_all(long_path(&self.mirror)).map_err(|e| explain_io_error(&e))?;
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
            match fs::metadata(long_path(&full)) {
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
        queue.mark_seen(self.items.iter().map(|i| i.path.as_str()));
        let stop = Arc::new(AtomicBool::new(false));
        // Адреса, добавленные добором: после загрузки войдут в список прогона.
        let added_log: Arc<Mutex<Vec<Item>>> = Arc::new(Mutex::new(Vec::new()));

        // Оптимизированный AIMD-контроллер (Д-135)
        let controller = {
            let shared = Arc::clone(&self.shared);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut bad_streak = 0usize;
                let mut baseline_ms = 0.0f64;
                while !stop.load(Ordering::Relaxed) {
                    for _ in 0..10 {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(200));
                    }
                    let bytes = shared.counters.win_bytes.swap(0, Ordering::Relaxed) as f64;
                    let files = shared.counters.win_files.swap(0, Ordering::Relaxed);
                    let n429 = shared.counters.win_429.swap(0, Ordering::Relaxed);
                    let n_over = shared.counters.win_overload.swap(0, Ordering::Relaxed);
                    let req_ms = shared.counters.win_req_ms.swap(0, Ordering::Relaxed);
                    let req_cnt = shared.counters.win_req_cnt.swap(0, Ordering::Relaxed);
                    let bps = bytes / 2.0;
                    *shared.counters.bps.lock().unwrap() = bps;
                    let avg_ms = if req_cnt > 0 {
                        req_ms as f64 / req_cnt as f64
                    } else {
                        0.0
                    };
                    let limit = shared.gate.limit();
                    let cap = shared.gate.cap();
                    let (next_limit, trigger_pause) = aimd_step(
                        limit,
                        cap,
                        &mut bad_streak,
                        &mut baseline_ms,
                        files,
                        bytes,
                        n429,
                        n_over,
                        avg_ms,
                    );
                    if trigger_pause {
                        shared.gate.set_limit(next_limit);
                        shared.log(crate::tr::pick(
                            format!("много 429 — пауза 120 с, потоков: {next_limit}"),
                            format!("many 429 — pause 120s, threads: {next_limit}"),
                        ));
                        shared.pause(120.0);
                    } else if next_limit != limit {
                        shared.gate.set_limit(next_limit);
                    }
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
                    // Проверка запускается по времени (120 с) или раньше, если
                    // прошло 500 файлов: на быстрой сети две минуты — это тысячи
                    // файлов, и пропущенное не вернуть (ФТ-152).
                    let mut last_seen = shared.counters.ok.load(Ordering::Relaxed);
                    for _ in 0..120 {
                        if stop.load(Ordering::Relaxed) || shared.cancel.load(Ordering::Relaxed) {
                            return;
                        }
                        std::thread::sleep(Duration::from_secs(1));
                        let now = shared.counters.ok.load(Ordering::Relaxed);
                        if now.saturating_sub(last_seen) >= 500 {
                            last_seen = now;
                            break;
                        }
                    }
                    let extra = format!(
                        "output=json&fl=timestamp,original,mimetype,statuscode,length,digest&filter=statuscode:200&collapse=urlkey&from={from}"
                    );
                    // Режим берём текущий: после 504 основной запрос уже перешёл на
                    // prefix, и добор должен идти так же.
                    let mode = match *shared.match_type.lock().unwrap() {
                        MatchType::Domain => "domain",
                        MatchType::Prefix => "prefix",
                    };
                    let query = format!(
                        "{}/cdx/search/cdx?url={}&matchType={mode}&{}",
                        shared.archive_base(),
                        urlencode(&host),
                        extra
                    );
                    let fetched = match shared.fetch_stream_until(&query, &stop, &mut |_, _| {}) {
                        Ok(fetched) => fetched,
                        Err(err) if err == "cancelled" => return,
                        Err(err) => {
                            // Та же человеческая причина, что и в остальных местах:
                            // сырой текст с кодом оставался только здесь.
                            let reason = explain_net(&err);
                            shared.log(crate::tr::pick(
                                format!("добор: запрос не удался — {reason}"),
                                format!("live: request failed — {reason}"),
                            ));
                            continue;
                        }
                    };
                    if fetched.status != 200 {
                        shared.log(crate::tr::pick(
                            format!("добор: индекс ответил {}", fetched.status),
                            format!("live: index answered {}", fetched.status),
                        ));
                        continue;
                    }
                    let mut best: HashMap<String, (String, String, String, u64, String)> =
                        HashMap::new();
                    for row in parse_rows(&fetched.body) {
                        fold_row(&mut best, &host, &row, &fetched.body, true);
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
                            shared.types.lock().unwrap().slot(item.kind)[1] += 1;
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
        let worker_count = self.workers.min(MAX_THREADS);
        for _ in 0..worker_count {
            let shared = Arc::clone(&self.shared);
            let queue = Arc::clone(&queue);
            handles.push(std::thread::spawn(move || {
                // Адрес берётся из очереди с ожиданием: пока идёт добор, очередь
                // может пополниться, и воркер не должен выходить раньше времени.
                // GateSlot захватывается только внутри сетевых шагов `fetch_one`.
                while let Some(item) = queue.pop() {
                    if shared.cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    shared.fetch_one(&item);
                }
            }));
        }

        // Ждём, пока вся работа выполнена: очередь может пополняться добором,
        // поэтому закрывать её сразу нельзя — добавленные адреса потерялись бы.
        let mut last_space_check = already;
        let mut last_meta_flush = already;
        loop {
            std::thread::sleep(Duration::from_millis(300));
            if self.shared.cancel.load(Ordering::Relaxed) {
                break;
            }
            let done_now = self.shared.counters.done.load(Ordering::Relaxed);
            if done_now.saturating_sub(last_meta_flush) >= 200 {
                last_meta_flush = done_now;
                self.save_content_types();
                self.save_no_snapshot();
                self.save_snapshots();
            }
            if done_now.saturating_sub(last_space_check) >= 500 {
                last_space_check = done_now;
                if let Some(free) = self.free_space()
                    && free < MIN_FREE_DURING_RUN
                {
                    let msg = crate::tr::pick(
                        format!(
                            "недостаточно места на диске во время загрузки (свободно {} МБ) — останавливаю прогон",
                            free / 1_048_576
                        ),
                        format!(
                            "not enough disk space during download ({} MB free) — stopping",
                            free / 1_048_576
                        ),
                    );
                    self.shared.log(msg.clone());
                    if let Ok(mut slot) = self.shared.disk_full_error.lock() {
                        *slot = Some(msg);
                    }
                    self.shared.cancel.store(true, Ordering::Relaxed);
                    break;
                }
            }
            if done_now >= queue.planned() {
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
        if let Ok(slot) = self.shared.disk_full_error.lock()
            && let Some(err) = slot.as_ref()
        {
            return Err(err.clone());
        }
        self.ensure_not_cancelled()
    }

    // ------------------------------------------------------------ build
    /// Собирает копию: переписывает ссылки, приводит кодировки, копирует файлы.
    ///
    /// Файлы обрабатываются параллельно; дерево зеркала обходится один раз; каталоги
    /// создаются один раз на путь; повторный прогон пропускает неизменённые файлы,
    /// беря цели ссылок из журнала сборки.
    /// Перенести счётчики одной сборки в накопленные (Д-56).
    ///
    /// Сборка после добора считается второй раз: без переноса отчёт удваивал
    /// «не страница», «перекодировано» и «адресов оставлено».
    fn flush_build_counters(&mut self) {
        self.foreign_total += self.foreign_built.swap(0, Ordering::Relaxed);
        self.left_total += self.shared.counters.left.swap(0, Ordering::Relaxed);
        self.script_left_total += self.shared.counters.script_left.swap(0, Ordering::Relaxed);
        if let Ok(mut current) = self.enc_stats.lock()
            && let Ok(mut total) = self.enc_total.lock()
        {
            for (label, count) in current.drain() {
                *total.entry(label).or_insert(0) += count;
            }
        }
    }

    /// Страницы, которые ссылались на указанные ключи (ФТ-153).
    ///
    /// Обратное отображение строится по журналу сборки: там у каждой страницы
    /// уже записан список целей `targets`, отдельная карта не нужна.
    fn pages_for(&self, keys: &HashSet<String>) -> HashSet<String> {
        // Журнал уже собран в память предыдущей сборкой: читать файл заново
        // незачем, а на большой копии это лишние мегабайты и время.
        let journal = self.journal.lock().unwrap();
        let mut pages = HashSet::new();
        for (rel, entry) in journal.iter() {
            if !entry.page {
                continue;
            }
            if entry.targets.iter().any(|key| keys.contains(key)) {
                pages.insert(rel.clone());
            }
        }
        pages
    }

    fn build(&self) -> Result<usize, String> {
        self.build_dirty(None)
    }

    /// Сборка с явным списком страниц, которые надо пересобрать (ФТ-153).
    ///
    /// Пропуск по размеру и времени изменения не даёт переписать страницу после
    /// добора: ссылка, оставленная абсолютной, так и осталась бы абсолютной.
    /// Поэтому «грязные» страницы проверку пропуска не проходят.
    fn build_dirty(&self, dirty: Option<&HashSet<String>>) -> Result<usize, String> {
        let files = self.mirror_list();
        // Отпечатки адресов: пропуск пересборки решается размером и отпечатком,
        // а не временем изменения файла (ФТ-146).
        let digests: HashMap<String, String> = self
            .items
            .iter()
            .filter(|item| !item.digest.is_empty())
            .map(|item| (item.path.clone(), item.digest.clone()))
            .collect();
        let index = Index::of(&files, &self.mirror);
        let total = files.len().max(1);

        let journal = self.load_journal();
        let incremental = !journal.is_empty();
        if !incremental && long_path(&self.site_dir).exists() {
            // remove_dir_all спотыкается о длинные пути Windows (глубокие каталоги
            // архивных сайтов): чистим своим обходом, добавляя префикс \\?\.
            remove_tree(&self.site_dir);
        }
        fs::create_dir_all(long_path(&self.site_dir)).map_err(|e| explain_io_error(&e))?;

        // Ведущий пробел (группа 1) обязателен: без него шаблон ловил JS-выражения
        // вида `location.href = x` и `k.src = r,` (П-Д2).
        let link_rx = Regex::new(
            r#"(?i)(\s)(href|src|data-src|data-bg|poster|action|data-original|srcset|data-srcset)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'<>`=]+))"#,
        )
        .unwrap();
        let css_rx = Regex::new(r#"url\(\s*["']?([^"')]+)["']?\s*\)"#).unwrap();
        let base_rx = Regex::new(r"(?i)<base\b[^>]*>").unwrap();
        // Сегменты HTML: комментарии, <script>...</script> и <style>...</style> обрабатываются
        // отдельно от обычной разметки, чтобы не портить inline-JS (П-Д2).
        let html_seg_rx = Regex::new(
            r"(?is)<!--.*?-->|<script\b([^>]*)>(.*?)</script\s*>|<style\b([^>]*)>(.*?)</style\s*>",
        )
        .unwrap();
        // Поддержка основного хоста, www и поддоменов (Д-142, П-8).
        let site_rx = Regex::new(&format!(
            r#"(?i)^(?:https?:)?//(?:[a-z0-9-]+\.)*{}(?::\d+)?(/[^"'\s<>()\\]*)"#,
            regex::escape(&self.host)
        ))
        .unwrap();
        // Для .js и <script> якорь ^ снимается, но сохраняется граница домена (Р3-2).
        let js_site_rx = Regex::new(&format!(
            r#"(?i)(^|["'`\s(,=:])((?:https?:)?//(?:[a-z0-9-]+\.)*{}(?::\d+)?(/[^"'\s<>()\\`]*))"#,
            regex::escape(&self.host)
        ))
        .unwrap();
        // Кэш склеенных минификаторов `/min/?f=a.css,b.css` (Д-143, П-9).
        let minifier_cache: Mutex<HashMap<String, Option<String>>> = Mutex::new(HashMap::new());

        let dirs: Mutex<HashSet<PathBuf>> = Mutex::new(HashSet::new());
        let done = AtomicUsize::new(0);
        let pages = AtomicUsize::new(0);
        let skipped = AtomicUsize::new(0);

        let results: Vec<(String, JournalEntry)> = self.in_pool(|| {
            files
                .par_iter()
                .filter_map(|src| {
                    if self.shared.cancel.load(Ordering::Relaxed) {
                        return None;
                    }
                    // Файл вне зеркала в этом списке — ошибка сборки, а не повод
                    // собрать его по абсолютному пути: `site_dir.join(abs)` подменил
                    // бы базу и перезаписал файл зеркала (найдено на код-ревью).
                    let Ok(rel) = src.strip_prefix(&self.mirror) else {
                        return None;
                    };
                    let rel = rel.to_string_lossy().replace('\\', "/");
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
                    let meta = fs::metadata(long_path(src)).ok()?;
                    let size = meta.len();
                    let journal_digest = digests.get(&rel).cloned().unwrap_or_default();
                    let mtime = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);

                    // Файл уже собран и в зеркале не менялся: переписывать нечего.
                    let dirty_page = dirty.is_some_and(|set| set.contains(&rel));
                    // Пропуск по размеру и отпечатку: время изменения ненадёжно —
                    // его меняет распаковка копии и перенос между дисками.
                    let same_content = prev_digest_ok(journal.get(&rel), &journal_digest);
                    if let Some(prev) = journal.get(&rel)
                        && prev.size == size
                        && same_content
                        && long_path(&dst).exists()
                        && !dirty_page
                    {
                        skipped.fetch_add(1, Ordering::Relaxed);
                        if prev.page {
                            pages.fetch_add(1, Ordering::Relaxed);
                        }
                        if prev.foreign {
                            self.foreign_built.fetch_add(1, Ordering::Relaxed);
                        }
                        self.bump_build(&done, total);
                        // Отметку проверки двигаем только для файлов, которые в этом
                        // прогоне действительно спрашивали у архива (набор `refresh`).
                        // Обновлять её на каждом пропуске нельзя: срок «проверить
                        // заново» (--refresh-days) тогда не наступает никогда и новый
                        // снимок остаётся незамеченным. Не обновлять вовсе тоже
                        // нельзя: запись навсегда остаётся просроченной, и каждый
                        // прогон заново качает весь объём зеркала.
                        let mut entry = prev.clone();
                        if self
                            .shared
                            .refresh
                            .lock()
                            .map(|set| set.contains(&rel))
                            .unwrap_or(false)
                        {
                            entry.checked = now_secs();
                        }
                        return Some((rel, entry));
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
                        if self.text_limit > 0 && size > self.text_limit as u64 {
                            // ФТ-142 / ФТ-143: огромный текстовый файл не читается в память
                            // и копируется без переписывания ссылок.
                            let copied = match fs::copy(long_path(src), long_path(&dst)) {
                                Ok(_) => true,
                                Err(e) => {
                                    self.log_write_error(&rel, &e);
                                    false
                                }
                            };
                            if copied {
                                self.text_skipped.fetch_add(1, Ordering::Relaxed);
                                if self.text_skipped_logged.fetch_add(1, Ordering::Relaxed) < 10 {
                                    self.shared.log(crate::tr::pick(
                                        format!("текст больше предела: не переписан — {rel}"),
                                        format!("text exceeds limit: copied as-is — {rel}"),
                                    ));
                                }
                                if is_page {
                                    pages.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            self.bump_build(&done, total);
                            if !copied {
                                return None;
                            }
                            return Some((
                                rel.clone(),
                                JournalEntry {
                                    size,
                                    digest: journal_digest.clone(),
                                    mtime,
                                    targets: Arc::new(Vec::new()),
                                    page: is_page,
                                    foreign: false,
                                    snapshot,
                                    checked: now_secs(),
                                },
                            ));
                        }
                        let mut data = match fs::read(long_path(src)) {
                            Ok(d) => d,
                            Err(e) => {
                                self.log_write_error(&rel, &e);
                                return None;
                            }
                        };
                        if data.starts_with(&[0x1F, 0x8B]) {
                            match gunzip_bytes_limited(&data, 64 * 1024 * 1024) {
                                Some(plain) => {
                                    data = plain;
                                    self.build_gunzipped.fetch_add(1, Ordering::Relaxed);
                                }
                                None => {
                                    self.build_gunzip_failed.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                        if !is_page && is_html_doc_head(&data) {
                            self.build_mismatched.fetch_add(1, Ordering::Relaxed);
                        }
                        if crate::encoding::looks_binary(&data) {
                            // под расширением .html в архиве бывает картинка или PDF:
                            // копируем байты как есть, иначе файл будет испорчен
                            let copied = match fs::copy(long_path(src), long_path(&dst)) {
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
                                    digest: journal_digest.clone(),
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
                        let rewrite_js = |chunk: &str| -> String {
                            js_site_rx
                                .replace_all(chunk, |caps: &regex::Captures| {
                                    let prefix = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                                    let full = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                                    let out = self.rewrite_ref_inner(
                                        full,
                                        src_dir,
                                        &index,
                                        &site_rx,
                                        Some(&minifier_cache),
                                        false,
                                    );
                                    if out == full {
                                        self.shared
                                            .counters
                                            .script_left
                                            .fetch_add(1, Ordering::Relaxed);
                                        if self.script_left_logged.fetch_add(1, Ordering::Relaxed)
                                            < 3
                                        {
                                            self.shared.log(crate::tr::pick(
                                                format!(
                                                    "скрипт: оставлен абсолютный адрес — {full}"
                                                ),
                                                format!("script: left absolute URL — {full}"),
                                            ));
                                        }
                                    }
                                    format!("{prefix}{out}")
                                })
                                .into_owned()
                        };
                        let mut rewrite_html_attrs = |chunk: &str| -> String {
                            link_rx
                                .replace_all(chunk, |caps: &regex::Captures| {
                                    let ws = &caps[1];
                                    let attr = &caps[2];
                                    // Кавычки: 3 — двойные, 4 — одинарные, 5 — без кавычек.
                                    let quoted = caps.get(5).is_none();
                                    let quote = if caps.get(3).is_some() {
                                        "\""
                                    } else if caps.get(4).is_some() {
                                        "'"
                                    } else {
                                        "\""
                                    };
                                    let value = caps
                                        .get(3)
                                        .or_else(|| caps.get(4))
                                        .or_else(|| caps.get(5))
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
                                                let new_url = self.rewrite_ref_inner(
                                                    url,
                                                    src_dir,
                                                    &index,
                                                    &site_rx,
                                                    Some(&minifier_cache),
                                                    true,
                                                );
                                                if rest.is_empty() {
                                                    new_url
                                                } else {
                                                    format!("{new_url} {}", rest.join(" "))
                                                }
                                            })
                                            .collect();
                                        return format!(
                                            "{ws}{attr}={quote}{}{quote}",
                                            items.join(", ")
                                        );
                                    }
                                    let new_value = self.rewrite_ref_inner(
                                        value,
                                        src_dir,
                                        &index,
                                        &site_rx,
                                        Some(&minifier_cache),
                                        true,
                                    );
                                    // Цели считаем по ИТОГОВОЙ ссылке, а для корневой ссылки,
                                    // превращённой в `https://...` (П-3), берём исходный ключ,
                                    // чтобы verify() и rescue() сохранили точность.
                                    let trimmed = value.trim();
                                    let min_paths = if new_value.starts_with("http") {
                                        minifier_param_paths(trimmed)
                                    } else {
                                        Vec::new()
                                    };
                                    if !min_paths.is_empty() {
                                        for mp in min_paths {
                                            collect(&format!("/{mp}"), quoted);
                                        }
                                    } else {
                                        let collect_src = if trimmed.starts_with('/')
                                            && !trimmed.starts_with("//")
                                            && new_value.starts_with("http")
                                        {
                                            trimmed
                                        } else {
                                            new_value.as_str()
                                        };
                                        collect(collect_src, quoted);
                                    }
                                    format!("{ws}{attr}={quote}{new_value}{quote}")
                                })
                                .into_owned()
                        };
                        let rewrite_css = |chunk: &str| -> String {
                            css_rx
                                .replace_all(chunk, |caps: &regex::Captures| {
                                    let url = &caps[1];
                                    format!(
                                        "url({})",
                                        self.rewrite_ref_inner(
                                            url,
                                            src_dir,
                                            &index,
                                            &site_rx,
                                            Some(&minifier_cache),
                                            true,
                                        )
                                    )
                                })
                                .into_owned()
                        };
                        if lower.ends_with(".js") {
                            text = rewrite_js(&text);
                        } else if lower.ends_with(".css") {
                            text = rewrite_css(&text);
                            text = crate::encoding::normalize_utf8(&text);
                        } else {
                            // HTML: переписываем атрибуты и <base> только вне <!--...-->, <script> и <style>.
                            let mut out = String::with_capacity(text.len());
                            let mut pos = 0usize;
                            for m in html_seg_rx.find_iter(&text) {
                                if m.start() > pos {
                                    let plain = &text[pos..m.start()];
                                    let plain = rewrite_html_attrs(plain);
                                    let plain = rewrite_css(&plain);
                                    let plain = base_rx.replace_all(&plain, "");
                                    out.push_str(&plain);
                                }
                                let seg = m.as_str();
                                if seg.starts_with("<!--") {
                                    out.push_str(seg);
                                } else if let Some(caps) = html_seg_rx.captures(seg) {
                                    if let Some(script_body) = caps.get(2) {
                                        let open_end = seg.find('>').map(|i| i + 1).unwrap_or(0);
                                        let close_start = seg.rfind('<').unwrap_or(seg.len());
                                        let open_tag = rewrite_html_attrs(&seg[..open_end]);
                                        let js_body = rewrite_js(script_body.as_str());
                                        out.push_str(&open_tag);
                                        out.push_str(&js_body);
                                        out.push_str(&seg[close_start..]);
                                    } else if let Some(style_body) = caps.get(4) {
                                        let open_end = seg.find('>').map(|i| i + 1).unwrap_or(0);
                                        let close_start = seg.rfind('<').unwrap_or(seg.len());
                                        out.push_str(&seg[..open_end]);
                                        out.push_str(&rewrite_css(style_body.as_str()));
                                        out.push_str(&seg[close_start..]);
                                    } else {
                                        out.push_str(seg);
                                    }
                                }
                                pos = m.end();
                            }
                            if pos < text.len() {
                                let tail = &text[pos..];
                                // Защита на случай незакрытого <script> без </script>.
                                let lower_tail = tail.to_ascii_lowercase();
                                if let Some(unclosed) = lower_tail.find("<script") {
                                    let head = &tail[..unclosed];
                                    let head = rewrite_html_attrs(head);
                                    let head = rewrite_css(&head);
                                    let head = base_rx.replace_all(&head, "");
                                    out.push_str(&head);
                                    out.push_str(&tail[unclosed..]);
                                } else {
                                    let tail = rewrite_html_attrs(tail);
                                    let tail = rewrite_css(&tail);
                                    let tail = base_rx.replace_all(&tail, "");
                                    out.push_str(&tail);
                                }
                            }
                            text = crate::encoding::normalize_utf8(&out);
                        }
                        // Страницу считаем записанной: иначе отчёт покажет файлы,
                        // которых на диске нет, а журнал запомнит их собранными.
                        let written = match fs::write(long_path(&dst), text.as_bytes()) {
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
                                digest: journal_digest.clone(),
                                mtime,
                                targets: Arc::new(targets),
                                page: is_page,
                                foreign: false,
                                snapshot,
                                checked: now_secs(),
                            },
                        ));
                    }

                    let mut handled_gzip = false;
                    if is_gzip_text_ext(&lower)
                        && size <= 16 * 1024 * 1024
                        && let Ok(raw) = fs::read(long_path(src))
                        && raw.starts_with(&[0x1F, 0x8B])
                    {
                        match gunzip_bytes_limited(&raw, 64 * 1024 * 1024) {
                            Some(plain) => {
                                if let Err(e) = fs::write(long_path(&dst), &plain) {
                                    self.log_write_error(&rel, &e);
                                    self.bump_build(&done, total);
                                    return None;
                                }
                                self.build_gunzipped.fetch_add(1, Ordering::Relaxed);
                                handled_gzip = true;
                            }
                            None => {
                                self.build_gunzip_failed.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    if !handled_gzip && let Err(e) = fs::copy(long_path(src), long_path(&dst)) {
                        self.log_write_error(&rel, &e);
                        self.bump_build(&done, total);
                        return None;
                    }
                    self.bump_build(&done, total);
                    Some((
                        rel.clone(),
                        JournalEntry {
                            size,
                            digest: journal_digest.clone(),
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
        let (site_files, unreadable) = walk_files_detailed(&self.site_dir);
        self.note_unreadable(&self.site_dir.clone(), &unreadable);
        let site_files = Arc::new(site_files);
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
        *self.journal.lock().unwrap() = journal_new;
        self.pages
            .store(pages.load(Ordering::Relaxed), Ordering::Relaxed);

        // Осиротевшие файлы прошлой копии убираем только при полном прогоне:
        // при ограничении числа адресов часть зеркала просто не скачана.
        if incremental && self.limit == 0 {
            self.drop_orphans(&files, &site_files);
            let (final_files, unreadable) = walk_files_detailed(&self.site_dir);
            self.note_unreadable(&self.site_dir.clone(), &unreadable);
            let final_files = Arc::new(final_files);
            self.site_files.store(final_files.len(), Ordering::Relaxed);
            *self.site_cache.lock().unwrap() = Some(Arc::clone(&final_files));
        }
        self.save_journal();

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
        // Ключ становится путём внутри копии и частью адреса для добора. Абсолютный
        // путь, буква диска или выход через «..» увели бы запись за пределы копии:
        // такие ссылки не проверяем и не добираем.
        // На Linux выход через «..» даёт абсолютный путь без буквы диска («/tmp»),
        // поэтому начальная косая проверяется отдельно: на Windows её снимает
        // `trim_start_matches`, и ключ в копии всегда относительный.
        if key.is_empty()
            || key.starts_with('/')
            || key.contains(':')
            || key.split('/').any(|part| part == "..")
        {
            return None;
        }
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
            if !rel.starts_with("_min/") && !keep.contains(&rel) {
                let _ = fs::remove_file(long_path(path));
            }
        }
    }

    fn journal_path(&self) -> PathBuf {
        self.data_dir.join("build.jsonl")
    }

    fn load_journal(&self) -> HashMap<String, JournalEntry> {
        let Ok(text) = fs::read_to_string(long_path(&self.journal_path())) else {
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
                "\",\"size\":{},\"mtime\":{},\"page\":{},\"foreign\":{},\"digest\":\"{}\",\"snapshot\":\"{}\",\"checked\":{},\"targets\":[",
                entry.size,
                entry.mtime,
                entry.page,
                entry.foreign,
                json_escape(&entry.digest),
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
            let _ = fs::create_dir_all(long_path(parent));
        }
        let tmp = path.with_extension("jsonl.part");
        if fs::write(long_path(&tmp), out).is_ok() {
            let _ = fs::rename(long_path(&tmp), long_path(&path));
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
    /// Счётчики кодировок за весь прогон: накопленное плюс текущая сборка.
    fn enc_totals(&self) -> HashMap<&'static str, usize> {
        let mut out = self.enc_total.lock().map(|m| m.clone()).unwrap_or_default();
        if let Ok(current) = self.enc_stats.lock() {
            for (label, count) in current.iter() {
                *out.entry(label).or_insert(0) += *count;
            }
        }
        out
    }

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
        let stats = self.enc_totals();
        let mut pairs: Vec<(&&'static str, &usize)> = stats.iter().collect();
        pairs.sort_by(|a, b| b.1.cmp(a.1));
        pairs
            .iter()
            .map(|(label, count)| format!("{label}: {count}"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    #[allow(dead_code)]
    fn rewrite_ref(&self, value: &str, src_dir: &str, index: &Index, site_rx: &Regex) -> String {
        self.rewrite_ref_inner(value, src_dir, index, site_rx, None, true)
    }

    fn is_included_url(&self, raw: &str) -> bool {
        if self.include_hosts.is_empty() {
            return false;
        }
        let parts = paths::parse(raw);
        let h = parts.host.trim_start_matches("www.");
        self.include_hosts
            .iter()
            .any(|inc| inc == &parts.host || inc == h)
    }

    /// Попробовать восстановить ссылку через минификатор `/min/?f=a.css,b.css` (Д-143, П-9).
    fn resolve_minifier(
        &self,
        url: &str,
        index: &Index,
        cache: Option<&Mutex<HashMap<String, Option<String>>>>,
    ) -> Option<String> {
        let items = minifier_param_paths(url);
        if items.is_empty() {
            return None;
        }
        if items.len() == 1 {
            return index.resolve(&items[0]);
        }
        let cache = cache?;
        if let Some(hit) = cache.lock().unwrap().get(url) {
            return hit.clone();
        }
        let ext = items
            .iter()
            .find_map(|p| {
                let low = p.to_ascii_lowercase();
                if low.ends_with(".css") {
                    Some("css")
                } else if low.ends_with(".js") {
                    Some("js")
                } else {
                    None
                }
            })
            .unwrap_or("css");
        let mut combined = Vec::new();
        let mut found_any = 0usize;
        for item in &items {
            if let Some(target) = index.resolve(item)
                && let Ok(bytes) = fs::read(long_path(&self.mirror.join(&target)))
            {
                let mut part = bytes;
                if part.starts_with(&[0x1F, 0x8B])
                    && let Some(plain) = gunzip_bytes_limited(&part, 16 * 1024 * 1024)
                {
                    part = plain;
                }
                combined.extend_from_slice(&part);
                combined.push(b'\n');
                found_any += 1;
            }
        }
        let res = if found_any > 0 {
            let bundle_rel = format!("_min/{}.{ext}", paths::fnv_hex(url));
            let bundle_dst = self
                .site_dir
                .join(bundle_rel.replace('/', std::path::MAIN_SEPARATOR_STR));
            if let Some(parent) = bundle_dst.parent() {
                let _ = fs::create_dir_all(long_path(parent));
            }
            if fs::write(long_path(&bundle_dst), &combined).is_ok() {
                self.minifier_built.fetch_add(1, Ordering::Relaxed);
                Some(bundle_rel)
            } else {
                None
            }
        } else {
            None
        };
        cache.lock().unwrap().insert(url.to_string(), res.clone());
        res
    }

    fn rewrite_ref_inner(
        &self,
        value: &str,
        src_dir: &str,
        index: &Index,
        site_rx: &Regex,
        minifier_cache: Option<&Mutex<HashMap<String, Option<String>>>>,
        promote_root_miss: bool,
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
        let from_root = raw.starts_with('/') && !raw.starts_with("//");
        let absolute = if from_root {
            Some(paths::normalize_url(
                &format!("https://{}{raw}", self.host),
                &self.host,
            ))
        } else if site_rx.is_match(raw) || self.is_included_url(raw) {
            Some(paths::normalize_url(raw, &self.host))
        } else {
            None
        };
        let Some(url) = absolute else {
            return value.to_string();
        };
        // Якорь — часть ссылки: без него переход ведёт на начало страницы
        // (оглавления и сноски в копии переставали работать).
        let anchor = raw.split_once('#').map(|(_, tail)| tail).unwrap_or("");
        let rel = paths::url_to_local_path_in(&url, &self.host);
        let resolved = index
            .resolve(&rel)
            .or_else(|| self.resolve_minifier(&url, index, minifier_cache));
        match resolved {
            Some(target) => {
                let mut out = rel_from(src_dir, &target);
                if !anchor.is_empty() {
                    out.push('#');
                    out.push_str(anchor);
                }
                out
            }
            None => {
                // Цель не найдена в копии: адрес остаётся в интернет-форме и
                // учитывается (ФТ-147, П-3 / Д-139). Для корневого `/...` вместо
                // тупикового пути от корня диска возвращаем `https://<host><path>`.
                self.shared.counters.left.fetch_add(1, Ordering::Relaxed);
                if from_root && promote_root_miss {
                    url
                } else {
                    value.to_string()
                }
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
            foreign: self.foreign_total + self.foreign_built.load(Ordering::Relaxed),
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
            .filter_map(|f| fs::metadata(long_path(f)).ok())
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
        let mut warns: Vec<String> = Vec::new();
        if old_total > 0 && old_n * 2 > old_total {
            warns.push(tr2(
                &format!(
                    "Больше половины файлов копии старше трёх лет ({old_n} из {old_total}): архив не содержит более свежих снимков этих адресов."
                ),
                &format!(
                    "More than half of the copy is older than three years ({old_n} of {old_total}): the archive holds no fresher snapshots for these addresses."
                ),
            ));
        }
        let has_css = site_list.iter().any(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("css"))
        });
        if !has_css && stats.pages > 0 && stats.broken > 0 {
            warns.push(tr2(
                "Внимание: в копии нет ни одного CSS-файла, страницы могут отображаться без оформления (проверьте --include-host для CDN-поддомена или увеличьте --rescue-budget).",
                "Warning: the copy has no CSS files; pages may render unstyled (check --include-host for a CDN subdomain or increase --rescue-budget).",
            ));
        }
        let fresh_warn = warns.join("<br>");
        let gunzip_ok = self.shared.counters.gunzipped.load(Ordering::Relaxed)
            + self.build_gunzipped.load(Ordering::Relaxed);
        let gunzip_bad = self.shared.counters.gunzip_failed.load(Ordering::Relaxed)
            + self.build_gunzip_failed.load(Ordering::Relaxed);
        let mismatched = self.shared.counters.mismatched.load(Ordering::Relaxed)
            + self.build_mismatched.load(Ordering::Relaxed);
        let min_built = self.minifier_built.load(Ordering::Relaxed);
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
                    &format!(
                        "Статическая копия: корзина, вход и AJAX исходного сайта не работают. Откройте кнопкой «Открыть копию» в программе или командой: site-restorer --serve \"{}\"",
                        show_path(&self.site_dir)
                    ),
                    &format!(
                        "Static copy: the original cart, sign-in and AJAX do not work. Open via the 'Open copy' button or run: site-restorer --serve \"{}\"",
                        show_path(&self.site_dir)
                    ),
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
            .replace(
                "{{t_archive}}",
                &tr2("Схема доступа к архиву", "Archive access scheme"),
            )
            .replace(
                "{{t_unreadable}}",
                &tr2("Каталогов не прочитано", "Directories not read"),
            )
            .replace("{{unreadable}}", &{
                let cnt = self.shared.counters.unreadable.load(Ordering::Relaxed);
                let details = self
                    .unreadable_paths
                    .lock()
                    .map(|l| l.clone())
                    .unwrap_or_default();
                if cnt == 0 || details.is_empty() {
                    cnt.to_string()
                } else {
                    let escaped: Vec<String> = details.iter().map(|s| html_escape(s)).collect();
                    format!("{cnt} ({})", escaped.join("; "))
                }
            })
            .replace(
                "{{archive}}",
                &format!(
                    "{} · {} {}",
                    self.shared.archive_scheme(),
                    self.shared.archive_requests(),
                    tr2("запросов", "requests")
                ),
            )
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
            .replace("{{t_build}}", &tr2("Сборка", "Build"))
            .replace(
                "{{build}}",
                &format!(
                    "site-restorer {} · CPU: {}",
                    crate::APP_VERSION,
                    crate::isa::describe()
                ),
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
            .replace("{{left}}", &{
                let all_left = self.left_total + self.shared.counters.left.load(Ordering::Relaxed);
                let sc_left = self.script_left_total
                    + self.shared.counters.script_left.load(Ordering::Relaxed);
                let skipped_txt = self.text_skipped.load(Ordering::Relaxed);
                let mut s = if sc_left > 0 {
                    format!(
                        "{all_left} ({} {sc_left})",
                        tr2("в скриптах:", "in scripts:")
                    )
                } else {
                    all_left.to_string()
                };
                if skipped_txt > 0 {
                    s.push_str(&format!(
                        " · {} {skipped_txt}",
                        tr2("текст > предела:", "text > limit:")
                    ));
                }
                s
            })
            .replace("{{index_total}}", &self.index_total.to_string())
            .replace(
                "{{index_pages}}",
                &if self.index_expected_pages > self.index_pages {
                    format!(
                        "{} {}",
                        self.index_pages,
                        tr2(
                            &format!("из {}", self.index_expected_pages),
                            &format!("of {}", self.index_expected_pages)
                        )
                    )
                } else {
                    self.index_pages.to_string()
                },
            )
            .replace("{{t_serve_title}}", &tr2("Как открыть копию", "How to open the copy"))
            .replace(
                "{{serve_cmd}}",
                &html_escape(&format!("site-restorer --serve \"{}\"", show_path(&self.out))),
            )
            .replace(
                "{{t_serve_note}}",
                &tr2(
                    "Нажмите «Открыть копию» в окне программы или выполните команду в консоли (внешний сервер не нужен):",
                    "Click 'Open copy' in the app window or run in a terminal (no external server needed):",
                ),
            )
            .replace("{{t_gunzipped}}", &tr2("Распаковано сжатых файлов", "Decompressed gzip files"))
            .replace(
                "{{gunzipped}}",
                &if gunzip_bad > 0 {
                    tr2(
                        &format!("{gunzip_ok} (не удалось: {gunzip_bad})"),
                        &format!("{gunzip_ok} (failed: {gunzip_bad})"),
                    )
                } else {
                    gunzip_ok.to_string()
                },
            )
            .replace(
                "{{t_mismatched}}",
                &tr2("Подменено содержимое ресурсов", "Mismatched resource content"),
            )
            .replace("{{mismatched}}", &mismatched.to_string())
            .replace(
                "{{t_minifier}}",
                &tr2("Собрано бандлов минификатора", "Minifier bundles assembled"),
            )
            .replace("{{minifier}}", &min_built.to_string())
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
                    // Сумма категорий сверяется с числом адресов ИНДЕКСА: items уже
                    // усечён --limit, поэтому при лимите сверка с ним всегда «N из N».
                    let ok = self.shared.counters.ok.load(Ordering::Relaxed);
                    let missing = self.shared.counters.missing.load(Ordering::Relaxed);
                    let errors = self.shared.counters.errors.load(Ordering::Relaxed);
                    let skipped = self.limit_skipped;
                    let sum = ok + missing + errors + skipped;
                    tr2(
                        &format!(
                            "{sum} из {} (скачано {ok}, пропущено {missing}, ошибок {errors}, по лимиту {skipped})",
                            self.index_total
                        ),
                        &format!(
                            "{sum} of {} (saved {ok}, missing {missing}, errors {errors}, over limit {skipped})",
                            self.index_total
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
        fs::write(long_path(&path), doc).map_err(|e| explain_io_error(&e))?;
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
        fs::create_dir_all(long_path(&self.data_dir)).map_err(|e| explain_io_error(&e))?;
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
        fs::write(long_path(&tmp), inv).map_err(|e| explain_io_error(&e))?;
        if fs::rename(long_path(&tmp), long_path(&path)).is_err() {
            // Windows заменяет файл при переименовании; отказ означает занятый
            // файл, поэтому повторяем после удаления (Д-103).
            let _ = fs::remove_file(long_path(&path));
            if fs::rename(long_path(&tmp), long_path(&path)).is_err() {
                let _ = fs::copy(long_path(&tmp), long_path(&path));
                let _ = fs::remove_file(long_path(&tmp));
            }
        }
        Ok(())
    }

    pub fn run(&mut self) -> Result<Outcome, String> {
        // Д-106: длина проверяется до обращения к сети, иначе пользователь узнает
        // о ней системным текстом в середине работы. Считается полный путь файла:
        // имя внутри копии добавляет до 160 символов.
        let probe = self
            .mirror
            .join("index.html")
            .to_string_lossy()
            .into_owned();
        let length = if cfg!(windows) {
            probe.chars().count()
        } else {
            probe.len()
        };
        let budget = path_budget();
        if length > budget {
            return Err(crate::tr::pick(
                format!(
                    "путь вывода слишком длинный: {length} при пределе {budget}; сократите --out"
                ),
                format!(
                    "output path is too long: {length} with a limit of {budget}; shorten --out"
                ),
            ));
        }
        fs::create_dir_all(long_path(&self.out)).map_err(|e| explain_io_error(&e))?;
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
        self.flush_build_counters();
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
            // В отчёте и в итоге — адреса, добранные на этапе сборки (ФТ-155).
            self.added_build = rescued;
            self.shared.log(crate::tr::pick(
                format!("rescue:ok:{rescued} — добрано по ссылкам, пересобираю"),
                format!("rescue:ok:{rescued} — rescued by links, rebuilding"),
            ));
            *self.mirror_cache.lock().unwrap() = None;
            self.mirror_files.store(0, Ordering::Relaxed);
            // Пересобираем только страницы, которые ссылались на добранное:
            // полная сборка на большой копии стоила бы минуты без пользы.
            let keys: HashSet<String> = broken.iter().cloned().collect();
            let pages = self.pages_for(&keys);
            if pages.is_empty() {
                self.build()?;
            } else {
                self.shared.log(crate::tr::pick(
                    format!("пересборка затронутых страниц: {}", pages.len()),
                    format!("rebuilding affected pages: {}", pages.len()),
                ));
                self.build_dirty(Some(&pages))?;
            }
            self.flush_build_counters();
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
            left: self.left_total + self.shared.counters.left.load(Ordering::Relaxed),
            index_total: self.index_total,
            index_pages: self.index_pages,
            index_truncated: self.index_truncated,
            index_failed: self.inv_failed,
            added_live: self.shared.counters.added_live.load(Ordering::Relaxed),
            added_build: self.added_build,
            oldest: self.snapshot_range().0,
            newest: self.snapshot_range().1,
            unreadable: self.shared.counters.unreadable.load(Ordering::Relaxed),
            archive: self.shared.archive_scheme().to_string(),
            archive_requests: self.shared.archive_requests(),
        })
    }

    /// Проверка свободного места до загрузки (Д-15). Оценка — по длине записей индекса
    /// с запасом: распаковка и вторая копия дерева.
    fn check_space(&self) -> Result<(), String> {
        let Some(free) = self.free_space() else {
            return Ok(());
        };
        let need = self.estimate_bytes();
        if need == 0 {
            self.shared.log(crate::tr::t(
                "место на диске: оценка объёма недоступна, предварительная проверка пропущена",
                "disk space: size estimate unavailable, pre-check skipped",
            ));
            return Ok(());
        }
        if need <= free {
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
        // Буфер один на весь обход: раньше `format!` стоял внутри двойного цикла
        // и создавал строку на каждую ссылку каждой страницы.
        let mut probe = String::with_capacity(64);
        for keys in targets.iter() {
            for key in keys.iter() {
                if Self::known_key(&known, key, &mut probe) {
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
        // П-5: сначала стили/скрипты/шрифты (`code`), затем картинки (`img`),
        // затем прочие ресурсы (`other`), и только потом страницы (`html`).
        out.sort_by_key(|k| {
            let rank = match paths::file_type(&format!("https://{}/{}", self.host, k)) {
                "code" => 0u8,
                "img" => 1,
                "other" => 2,
                _ => 3,
            };
            (rank, k.clone())
        });
        out
    }

    /// Есть ли ключ в наборе файлов копии — как есть или как каталог с `index.html`.
    ///
    /// `probe` — переиспользуемый буфер: проверка идёт во внутреннем цикле по всем
    /// ссылкам всех страниц, и строка на каждый вызов там недопустима.
    fn known_key(known: &HashSet<String>, key: &str, probe: &mut String) -> bool {
        if known.contains(key) {
            return true;
        }
        probe.clear();
        probe.push_str(key);
        probe.push_str("/index.html");
        known.contains(probe.as_str())
    }

    /// Добор по ссылкам (ФТ-128, Д-136): отсутствующие цели догружаются снимком по их адресу.
    /// Пул до 4 потоков в пределах `rescue_limit` адресов и бюджета `rescue_budget`.
    fn rescue(&mut self, missing: &[String]) -> usize {
        if self.rescue_limit == 0 || missing.is_empty() {
            return 0;
        }
        let target_total = missing.len().min(self.rescue_limit);
        let n_threads = self.workers.clamp(1, 4).min(target_total);
        let next_idx = AtomicUsize::new(0);
        let done = AtomicUsize::new(0);
        let budget_logged = AtomicBool::new(false);
        let started = Instant::now();
        let cancel = self.cancel_token();
        let slice = &missing[..target_total];

        std::thread::scope(|s| {
            for _ in 0..n_threads {
                let next_idx = &next_idx;
                let done = &done;
                let budget_logged = &budget_logged;
                let cancel = &cancel;
                let this = &*self;
                s.spawn(move || {
                    loop {
                        if cancel.load(Ordering::Relaxed) {
                            return;
                        }
                        if this.rescue_budget > 0
                            && started.elapsed().as_secs() >= this.rescue_budget
                        {
                            if !budget_logged.swap(true, Ordering::Relaxed) {
                                let rescued_now = done.load(Ordering::Relaxed);
                                this.shared.log(crate::tr::pick(
                                    format!(
                                        "добор по ссылкам остановлен по времени: {} с, добрано {rescued_now} из {target_total}",
                                        this.rescue_budget
                                    ),
                                    format!(
                                        "rescue: budget {} s spent, rescued {rescued_now} of {target_total}",
                                        this.rescue_budget
                                    ),
                                ));
                            }
                            return;
                        }
                        let idx = next_idx.fetch_add(1, Ordering::Relaxed);
                        if idx >= target_total {
                            return;
                        }
                        let key = &slice[idx];
                        let url = format!("https://{}/{}", this.host, key.trim_start_matches('/'));
                        let rel = paths::url_to_local_path_in(&url, &this.host);
                        let item = Item {
                            kind: paths::file_type(&url),
                            ts: String::new(),
                            orig: url.clone(),
                            mime: String::new(),
                            path: rel,
                            url,
                            len: 0,
                            digest: String::new(),
                            same_as: String::new(),
                        };
                        if this.fetch_target(&item) {
                            done.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
        });

        let total_done = done.load(Ordering::Relaxed);
        if !missing.is_empty() && total_done == 0 && console_verbose() {
            // Ни один адрес из добора не получен: это не машинная метка, а факт,
            // который надо объяснить словами (визуальный тест 15.11.2026).
            self.shared.log(crate::tr::pick(
                format!("добор по ссылкам: из {target_total} адресов не получен ни один"),
                format!("link rescue: none of {target_total} addresses could be fetched"),
            ));
        }
        total_done
    }

    /// Тихое получение снимка по адресу: без счётчиков прогресса, только в зеркало.
    /// Дата полученного снимка записывается так же, как при обычной загрузке (ФТ-129).
    fn fetch_target(&self, item: &Item) -> bool {
        // Добор делает одну попытку по свежему снимку: цепочка из шести запросов
        // на каждую из сотен целей — это тысячи запросов к архиву (ФТ-128).
        let target = SnapshotMode::Newest.target(&item.ts);
        match self.shared.attempt(item, &target, self.stub_bytes) {
            Attempt::Saved { memento } => {
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
/// Шаг адаптивного управления конкурентностью (Д-135).
///
/// Возвращает `(новый_лимит, нужна_ли_пауза_120с)`.
/// Отличие от прежней версии:
/// - не снижает лимит из-за падения байт/с на мелких файлах при нормальной задержке;
/// - сбрасывает серию 429 на любом чистом окне;
/// - реагирует на перегрузку `5xx`/таймаутов/`10061` (`×3/4`) и резкий рост задержки.
#[allow(clippy::too_many_arguments)]
fn aimd_step(
    limit: usize,
    cap: usize,
    bad_streak: &mut usize,
    baseline_ms: &mut f64,
    files: usize,
    bytes: f64,
    n429: usize,
    n_over: usize,
    avg_ms: f64,
) -> (usize, bool) {
    let cap = cap.clamp(1, MAX_THREADS);
    let min_l = MIN_THREADS.min(cap);
    if n429 > 0 {
        *bad_streak += 1;
        if *bad_streak >= 3 {
            *bad_streak = 0;
            return (min_l, true);
        }
        return ((limit / 2).clamp(1, cap), false);
    }
    *bad_streak = 0;
    if n_over > 0 {
        let next = if limit > 1 {
            (limit * 3 / 4).min(limit - 1).max(1)
        } else {
            1
        };
        return (next.clamp(1, cap), false);
    }
    if avg_ms > 0.0 {
        if *baseline_ms <= 0.0 {
            *baseline_ms = avg_ms.max(500.0);
        } else if avg_ms <= *baseline_ms * 1.5 {
            *baseline_ms = (*baseline_ms * 0.8 + avg_ms * 0.2).max(500.0);
        }
        if avg_ms > *baseline_ms * 2.5 && avg_ms > 3000.0 {
            return (limit.saturating_sub(1).clamp(1, cap), false);
        }
    }
    if files > 0 || bytes > 0.0 {
        return ((limit + 1).clamp(1, cap), false);
    }
    (limit.clamp(1, cap), false)
}

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
    if !long_path(path).exists() {
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
    if !long_path(path).exists() {
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

/// Экранирование для HTML-отчёта.
fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Экранирование для JSON-строк инвентаря и CLI-итога.
pub(crate) fn json_escape(text: &str) -> String {
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
pub(crate) fn stamp_now() -> String {
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

/// Пауза из заголовка `Retry-After` (Д-99, RFC 9110 §10.2.3).
///
/// Допустимы две формы: число секунд и HTTP-дата. Раньше разбиралось только число,
/// и дату сервера программа молча игнорировала — шла дальше вопреки просьбе
/// подождать. Потолок общий для обеих форм: он защищает от `inf`, от 1e300
/// (паника в `Duration::from_secs_f64`) и от даты на годы вперёд.
fn retry_after_secs(text: &str) -> Option<f64> {
    let value = text.trim();
    if let Ok(number) = value.parse::<f64>() {
        return number
            .is_finite()
            .then(|| number.clamp(0.0, MAX_RETRY_WAIT));
    }
    let stamp = stamp_from_http_date(value)?;
    let target = stamp_to_secs(&stamp)?;
    let now = now_secs();
    // Дата в прошлом — ждать нечего, но и отрицательной паузы быть не может.
    Some((target as f64 - now as f64).clamp(0.0, MAX_RETRY_WAIT))
}

/// Секунды Unix из метки архива `ГГГГММДДччммсс` (UTC).
fn stamp_to_secs(stamp: &str) -> Option<u64> {
    let year: i64 = stamp.get(0..4)?.parse().ok()?;
    let month: u64 = stamp.get(4..6)?.parse().ok()?;
    let day: u64 = stamp.get(6..8)?.parse().ok()?;
    let hour: u64 = stamp.get(8..10)?.parse().ok()?;
    let minute: u64 = stamp.get(10..12)?.parse().ok()?;
    let second: u64 = stamp.get(12..14)?.parse().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(days as u64 * 86_400 + hour * 3600 + minute * 60 + second)
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
    // Срез по байтам паниковал, если в дате оказывались многобайтовые символы
    // (повреждённый журнал или чужой ответ). `get` возвращает None на границе
    // символа, и дата просто остаётся как есть.
    let (Some(year), Some(month), Some(day)) = (ts.get(..4), ts.get(4..6), ts.get(6..8)) else {
        return ts.to_string();
    };
    format!("{year}-{month}-{day}")
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
    let Ok(text) = fs::read_to_string(long_path(file)) else {
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

/// Строка ответа CDX: границы полей внутри тела, без копий (Д-64).
///
/// Раньше тело разворачивалось в `Vec<char>` (четыре байта на символ) и в
/// `Vec<String>` с копией каждого поля: на странице в 20 000 строк это 8 МБ
/// символов и 120 000 аллокаций, хотя значения нужны на несколько микросекунд.
/// Здесь хранятся только смещения, а значения берутся из тела по требованию.
pub(crate) struct Row {
    spans: [Span; MAX_FIELDS],
    count: u8,
}

/// Границы одного поля: содержимое между кавычками.
#[derive(Clone, Copy, Default)]
struct Span {
    start: u32,
    end: u32,
}

/// Сколько полей запроса помещается в одну строку.
const MAX_FIELDS: usize = 6;

impl Row {
    /// Поле по номеру: срез тела, если экранирования нет, иначе собранная строка.
    pub(crate) fn field<'a>(&self, body: &'a [u8], idx: usize) -> Option<Cow<'a, str>> {
        if idx >= usize::from(self.count).min(MAX_FIELDS) {
            return None;
        }
        let span = self.spans[idx];
        let raw = body.get(span.start as usize..span.end as usize)?;
        Some(decode_field(raw))
    }

    /// Сколько полей пришло в строке (может быть больше, чем хранится).
    pub(crate) fn len(&self) -> usize {
        usize::from(self.count)
    }
}

/// Разбор ответа CDX (массив массивов строк) без внешних библиотек.
pub(crate) fn parse_rows(body: &[u8]) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut from = 0usize;
    while from < body.len() {
        let Some(open) = body[from..].iter().position(|&b| b == b'[') else {
            break;
        };
        let mut j = from + open + 1;
        while j < body.len() && body[j].is_ascii_whitespace() {
            j += 1;
        }
        // Внешний массив — контейнер; строка данных начинается сразу с кавычки.
        if body.get(j) != Some(&b'"') {
            from += open + 1;
            continue;
        }
        let (row, next) = read_row(body, j);
        from = next.max(from + open + 1);
        // Одно поле — это законная строка запроса `fl=timestamp`: по ней
        // собираются даты снимков для добора. Шапка отличается не числом полей,
        // а первым полем "timestamp".
        if let Some(row) = row
            && row.len() >= 1
            && row.field(body, 0).is_none_or(|ts| ts != "timestamp")
        {
            rows.push(row);
        }
    }
    rows
}

/// Прочитать одну строку данных, начиная с открывающей кавычки первого поля.
fn read_row(body: &[u8], mut j: usize) -> (Option<Row>, usize) {
    let mut spans = [Span::default(); MAX_FIELDS];
    let mut count: u8 = 0;
    loop {
        while j < body.len() && (body[j].is_ascii_whitespace() || body[j] == b',') {
            j += 1;
        }
        match body.get(j) {
            // Тело кончилось до закрывающей скобки: ответ пришёл обрезанным.
            // Строку не принимаем — иначе обрывок адреса станет адресом.
            None => return (None, j),
            Some(b']') => return (build_row(spans, count), j + 1),
            Some(b'"') => {
                let start = j + 1;
                let end = closing_quote(body, start);
                let (Ok(start32), Ok(end32)) = (u32::try_from(start), u32::try_from(end)) else {
                    // Тело больше 4 ГБ в ответе индекса не встречается; если
                    // встретится — строку пропускаем, а не портим смещения.
                    return (None, end.saturating_add(1));
                };
                if usize::from(count) < MAX_FIELDS {
                    spans[usize::from(count)] = Span {
                        start: start32,
                        end: end32,
                    };
                }
                count = count.saturating_add(1);
                j = end.saturating_add(1);
            }
            Some(_) => j += 1,
        }
    }
}

fn build_row(spans: [Span; MAX_FIELDS], count: u8) -> Option<Row> {
    (count > 0).then_some(Row { spans, count })
}

/// Позиция закрывающей кавычки поля; экранированные кавычки пропускаются.
fn closing_quote(body: &[u8], from: usize) -> usize {
    let mut k = from;
    while k < body.len() {
        let Some(pos) = body[k..].iter().position(|&b| b == b'"') else {
            return body.len();
        };
        k += pos;
        // Кавычка экранирована, если перед ней нечётное число обратных косых.
        let mut backslashes = 0usize;
        let mut q = k;
        while q > from && body[q - 1] == b'\\' {
            backslashes += 1;
            q -= 1;
        }
        if backslashes.is_multiple_of(2) {
            return k;
        }
        k += 1;
    }
    body.len()
}

fn decode_field(raw: &[u8]) -> Cow<'_, str> {
    if raw.contains(&b'\\') {
        return Cow::Owned(unescape(raw));
    }
    match std::str::from_utf8(raw) {
        Ok(text) => Cow::Borrowed(text),
        Err(_) => Cow::Owned(String::from_utf8_lossy(raw).into_owned()),
    }
}

/// Снять экранирование JSON. Встречается редко (кавычка или обратная косая в
/// адресе), поэтому идёт отдельным путём и не влияет на обычный разбор.
fn unescape(raw: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(raw.len());
    let mut i = 0usize;
    while i < raw.len() {
        if raw[i] != b'\\' || i + 1 >= raw.len() {
            out.push(raw[i]);
            i += 1;
            continue;
        }
        i += 1;
        match raw[i] {
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'r' => out.push(b'\r'),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'u' => {
                if let Some((ch, used)) = hex_escape(&raw[i + 1..]) {
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                    i += used;
                } else {
                    out.push(b'u');
                }
            }
            other => out.push(other),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Экранированный символ `\uXXXX` (в том числе пара суррогатов).
///
/// Возвращает символ и число байтов после `u`, которые он занял: четыре для
/// обычного символа, десять для пары вида `\uD83D\uDE00` — иначе эмодзи в
/// адресе распадалось бы на два нечитаемых куска.
fn hex_escape(tail: &[u8]) -> Option<(char, usize)> {
    let code = u32::from(hex4(tail.get(..4)?)?);
    if (0xD800..=0xDBFF).contains(&code) {
        // Старший суррогат: сразу за ним должен идти `\u` и младший суррогат.
        let rest = tail.get(4..)?;
        let low = rest
            .strip_prefix(b"\\u")
            .and_then(|tail| hex4(tail.get(..4)?));
        if let Some(low) = low {
            let low = u32::from(low);
            if (0xDC00..=0xDFFF).contains(&low) {
                let combined = 0x1_0000 + ((code - 0xD800) << 10) + (low - 0xDC00);
                return char::from_u32(combined).map(|ch| (ch, 10));
            }
        }
        return None;
    }
    char::from_u32(code).map(|ch| (ch, 4))
}

/// Четыре шестнадцатеричные цифры в единицу UTF-16.
fn hex4(digits: &[u8]) -> Option<u16> {
    if digits.len() < 4 {
        return None;
    }
    let mut code = 0u16;
    for &b in &digits[..4] {
        let digit = match b {
            b'0'..=b'9' => u16::from(b - b'0'),
            b'a'..=b'f' => u16::from(b - b'a') + 10,
            b'A'..=b'F' => u16::from(b - b'A') + 10,
            _ => return None,
        };
        code = code * 16 + digit;
    }
    Some(code)
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
///
/// Здесь же убираются `.` и `..`: префикс `\\?\` отключает их разбор на стороне
/// системы (Д-105), поэтому `--out ..\site` иначе превратился бы в нерабочий путь.
fn absolute_dir(path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(_) => path.to_path_buf(),
        }
    };
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for part in joined.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                // Выше корня не поднимаемся, и сам корень не снимаем: без него
                // `C:\..\x` стал бы `C:x` — путём относительно текущего каталога
                // диска, и копия уехала бы в чужое место (найдено на код-ревью).
                let at_root = matches!(
                    parts.last(),
                    Some(last) if last.to_string_lossy().ends_with(':')
                        || last.to_string_lossy() == "\\"
                );
                if parts.len() > 2 && !at_root {
                    parts.pop();
                }
            }
            other => parts.push(other.as_os_str().to_os_string()),
        }
    }
    let mut out = PathBuf::new();
    for part in parts {
        out.push(part);
    }
    let out = if out.as_os_str().is_empty() {
        joined
    } else {
        out
    };
    canonical_dir(&out)
}

/// Полная форма каталога: раскрытые короткие имена 8.3 и уже добавленный префикс
/// длинного пути (`canonicalize` на Windows возвращает `\\?\C:\...`).
///
/// Зачем: префикс `\\?\` отключает на стороне системы и разбор `.`/`..`, и
/// подстановку коротких имён, поэтому `C:\Users\7636~1\Temp` с префиксом
/// становится недоступным путём. Если каталога ещё нет, приводится ближайший
/// существующий предок, а остаток имени дописывается как есть.
fn canonical_dir(path: &Path) -> PathBuf {
    if let Ok(full) = fs::canonicalize(path) {
        return full;
    }
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path.to_path_buf();
    loop {
        let Some(parent) = cur.parent().map(Path::to_path_buf) else {
            return path.to_path_buf();
        };
        if let Some(name) = cur.file_name() {
            missing.push(name.to_os_string());
        }
        cur = parent;
        if let Ok(base) = fs::canonicalize(&cur) {
            let mut out = base;
            for name in missing.iter().rev() {
                out.push(name);
            }
            return out;
        }
        if cur.parent().is_none() {
            return path.to_path_buf();
        }
    }
}

/// Порядок адресов для `--limit` (Д-103, Д-141 / П-7).
///
/// 1. Главная страница.
/// 2. Ключевые ассеты (`code`: CSS/JS/шрифты, сначала без `?query`) — чтобы у пробы была вёрстка.
/// 3. Равномерная выборка (round-robin) по корневым разделам (сначала чистые URL, затем `__q_`).
fn limit_order(items: Vec<Item>) -> Vec<Item> {
    let mut front: Vec<Item> = Vec::new();
    let mut code_assets: Vec<Item> = Vec::new();
    let mut root_items: Vec<Item> = Vec::new();
    let mut sections: HashMap<String, Vec<Item>> = HashMap::new();

    for item in items {
        let rest = item.url.splitn(4, '/').nth(3).unwrap_or("");
        if (rest.is_empty() || rest == "index.html") && !item.path.contains("__q_") {
            front.push(item);
        } else if item.kind == "code" {
            code_assets.push(item);
        } else if !rest.contains('/') {
            root_items.push(item);
        } else {
            let key = rest.split('/').next().unwrap_or("").to_string();
            sections.entry(key).or_default().push(item);
        }
    }

    let clean_first = |list: &mut Vec<Item>| {
        list.sort_by(|a, b| {
            let qa = a.path.contains("__q_");
            let qb = b.path.contains("__q_");
            (qa, &a.url).cmp(&(qb, &b.url))
        });
    };
    front.sort_by(|a, b| a.url.cmp(&b.url));
    clean_first(&mut code_assets);
    clean_first(&mut root_items);

    let mut sec_keys: Vec<(usize, String)> =
        sections.iter().map(|(k, v)| (v.len(), k.clone())).collect();
    sec_keys.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let mut buckets: Vec<VecDeque<Item>> = Vec::with_capacity(sec_keys.len());
    for (_, k) in sec_keys {
        let mut list = sections.remove(&k).unwrap_or_default();
        clean_first(&mut list);
        buckets.push(list.into());
    }

    let total = front.len()
        + code_assets.len()
        + root_items.len()
        + buckets.iter().map(|b| b.len()).sum::<usize>();
    let mut out = Vec::with_capacity(total);
    out.extend(front);
    out.extend(root_items);
    // Часть CSS/JS ставим сразу за главной и корневыми страницами, чтобы вёрстка
    // попадала в любой `--limit`, а остальные равномерно чередуются с разделами.
    let mut code_q: VecDeque<Item> = code_assets.into();
    let head_code = (total / 8).clamp(32, 200).min(code_q.len());
    for _ in 0..head_code {
        if let Some(c) = code_q.pop_front() {
            out.push(c);
        }
    }
    loop {
        let mut added = false;
        if let Some(c) = code_q.pop_front() {
            out.push(c);
            added = true;
        }
        for b in &mut buckets {
            if let Some(item) = b.pop_front() {
                out.push(item);
                added = true;
            }
        }
        if !added {
            break;
        }
    }
    out
}

/// Стоит ли менять схему архива из-за этой ошибки (Д-97, уточнено 14.11.2026).
///
/// Таймаут чтения означает «сервер отвечает медленно», а не «адрес недоступен»,
/// поэтому схему из-за него не меняем. Соединение, DNS, TLS — меняем.
fn scheme_switch_worthwhile(err: &str) -> bool {
    let low = err.to_lowercase();
    // Любой таймаут — признак перегрузки или медленного канала, а не закрытого
    // порта: 14.11.2026 на живом прогоне `timeout: connect` уводил программу на
    // http, где порт 80 недоступен вовсе, и ошибок только прибавлялось.
    if low.contains("timeout") || low.contains("timed out") {
        return false;
    }
    // Обрыв посреди тела («connection closed while reading») сюда не входит: сервер
    // принял запрос и начал отвечать, значит адрес живой — помогает повтор, а не
    // смена схемы.
    // Ключи намеренно узкие: «connection closed while reading» — обрыв посреди тела,
    // адрес при этом живой, помогает повтор, а не смена схемы. Поэтому здесь
    // «connection refused», а не «connect».
    [
        "connection refused",
        "connect error",
        "connect:",
        "10061",
        "10051",
        "dns",
        "no route",
        "unreachable",
    ]
    .iter()
    .any(|key| low.contains(key))
}

/// Понятная причина сетевой ошибки.
///
/// Текст берётся из системной библиотеки, поэтому на Windows и Linux он разный и
/// с кодом. Рядом ставим человеческую формулировку, код оставляем в скобках:
/// журнал читают, а не диагностируют по нему сетевой стек.
fn explain_net(err: &str) -> String {
    let low = err.to_lowercase();
    let phrase = if low.contains("10061") || low.contains("connection refused") {
        Some(crate::tr::pick(
            "архив отклонил подключение".to_string(),
            "the archive refused the connection".to_string(),
        ))
    } else if low.contains("10054") || low.contains("reset by peer") {
        Some(crate::tr::pick(
            "соединение сброшено архивом".to_string(),
            "the archive reset the connection".to_string(),
        ))
    } else if low.contains("10060") || low.contains("timeout") || low.contains("timed out") {
        Some(crate::tr::pick(
            "архив не ответил за отведённое время".to_string(),
            "the archive did not answer in time".to_string(),
        ))
    } else if low.contains("10051") || low.contains("unreachable") || low.contains("no route") {
        Some(crate::tr::pick(
            "нет маршрута к архиву".to_string(),
            "no route to the archive".to_string(),
        ))
    } else if low.contains("certificate")
        || low.contains("invalidpeer")
        || low.contains("unknownissuer")
        || low.contains("expired")
        || low.contains("tls")
    {
        Some(crate::tr::pick(
            "ошибка сертификата архива — проверьте дату системы, прокси или антивирус".to_string(),
            "archive certificate error — check system clock, proxy or antivirus".to_string(),
        ))
    } else if low.contains("dns") || low.contains("resolve") {
        Some(crate::tr::pick(
            "имя архива не разрешается".to_string(),
            "the archive name does not resolve".to_string(),
        ))
    } else {
        None
    };
    // Код берём из хвоста «… (os error 10061)»: цифры сразу после этой пометки.
    let code = err.rfind("os error ").map(|i| {
        err[i + "os error ".len()..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
    });
    let code = code.filter(|c| !c.is_empty());
    match (phrase, code) {
        (Some(phrase), Some(code)) => format!("{phrase} ({code})"),
        (Some(phrase), None) => phrase.to_string(),
        (None, _) => err.to_string(),
    }
}

/// Предел длины полного пути для текущей платформы (Д-106).
///
/// Windows: манифест объявляет `longPathAware`, поэтому предел — 32 767 знаков,
/// берём с запасом 32 000. Linux: `PATH_MAX` = 4096 байт, оставляем 4000 на служебные
/// имена внутри копии.
fn path_budget() -> usize {
    if cfg!(windows) { 32_000 } else { 4_000 }
}

/// Текст ошибки записи с понятной причиной (ФТ-45, Д-106).
///
/// Коды «имя или расширение слишком длинны» (Windows 206, Linux 36) раньше
/// доходили до пользователя системной строкой без подсказки.
fn explain_io_error(err: &std::io::Error) -> String {
    // 206 — `ERROR_FILENAME_EXCED_RANGE` (Windows), 36 — `ENAMETOOLONG` в Linux.
    // На macOS код 36 означает совсем другое, поэтому Linux проверяется явно.
    let too_long = match err.raw_os_error() {
        Some(206) => cfg!(windows),
        Some(36) => cfg!(target_os = "linux"),
        _ => false,
    };
    if too_long {
        return crate::tr::pick(
            format!("путь слишком длинный для этой системы ({err}); сократите папку вывода"),
            format!("the path is too long for this system ({err}); shorten the output folder"),
        );
    }
    err.to_string()
}

/// Первая строка журнала: `#journal <формат> <версия> <хост>`.
fn journal_label(line: Option<&str>) -> Option<String> {
    let line = line?.trim();
    line.starts_with("#journal ").then(|| line.to_string())
}

/// Префикс длинного пути Windows: без него файловые операции не видят пути длиннее 260 символов.
pub(crate) fn long_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if cfg!(windows) && path.is_absolute() && !text.starts_with(r"\\?\") {
        let win = text.replace('/', r"\");
        // Сетевой путь требует своей формы: \\?\UNC\server\share, иначе из сетевого пути получалось \\?\\\server и операции с файлами молча не работали.
        if let Some(rest) = win.strip_prefix(r"\\") {
            return PathBuf::from(format!(r"\\?\UNC\{rest}"));
        }
        PathBuf::from(format!(r"\\?\{win}"))
    } else {
        path.to_path_buf()
    }
}

/// Путь для показа человеку: без служебного префикса длинных путей.
///
/// `long_path` добавляет `\\?\`, и без этой очистки префикс попадал бы в
/// консоль, отчёт и журнал.
pub fn show_path(path: &Path) -> String {
    let text = path.display().to_string();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_string()
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

/// Конец списка целей: `]` внутри строки (путь вида `a[1].html`) списком не считается.
fn targets_end(text: &str) -> Option<usize> {
    let mut in_str = false;
    let mut escaped = false;
    for (index, ch) in text.char_indices() {
        match ch {
            _ if escaped => escaped = false,
            '\\' if in_str => escaped = true,
            '"' => in_str = !in_str,
            ']' if !in_str => return Some(index),
            _ => {}
        }
    }
    None
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
    let digest = json_field(line, "digest").unwrap_or_default();
    let mut targets = Vec::new();
    if let Some(start) = line.find("\"targets\":[") {
        let rest = &line[start + "\"targets\":[".len()..];
        if let Some(end) = targets_end(rest) {
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
            digest,
            mtime,
            targets: Arc::new(targets),
            page,
            foreign,
            snapshot: json_field(line, "snapshot").unwrap_or_default(),
            checked: json_number(line, "checked").unwrap_or(0),
        },
    ))
}

/// Обход дерева: список файлов и число каталогов, которые прочитать не удалось.
///
/// Тип записи берётся из самого каталога (`DirEntry::file_type`), без отдельного
/// `stat` на каждый файл — на Windows это снимает два системных вызова с каждой
/// записи и ускоряет обход на порядок.
///
/// Три правила, каждое из доработок 3.6.3:
/// * каталог читается через префикс длинного пути (Д-105) — иначе на системе без
///   опции длинных путей поддеревья молча выпадали;
/// * каталог-ссылка (junction) не обходится (Д-103) — иначе можно зациклиться и
///   выйти за пределы копии;
/// * число непрочитанных каталогов возвращается наружу (Д-107): раньше ошибка
///   терялась в `continue`, и числа отчёта занижались без следа.
fn walk_files_detailed(root: &Path) -> (Vec<PathBuf>, Vec<(PathBuf, String)>) {
    let mut out = Vec::new();
    let mut unreadable = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut seen: HashSet<PathBuf> = HashSet::new();
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(long_path(&dir)) {
            Ok(entries) => entries,
            Err(err) => {
                unreadable.push((dir, err.to_string()));
                continue;
            }
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_symlink() {
                continue;
            }
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
    (out, unreadable)
}

#[cfg(test)]
fn walk_files(root: &Path) -> (Vec<PathBuf>, usize) {
    let (files, bad) = walk_files_detailed(root);
    (files, bad.len())
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
<div style="background:#eff6ff;border:1px solid #bfdbfe;border-radius:10px;padding:12px 16px;margin-top:16px">
<div style="font-weight:600;margin-bottom:4px">{{t_serve_title}}</div>
<div style="color:#374151;font-size:14px;margin-bottom:6px">{{t_serve_note}}</div>
<code style="display:inline-block;background:#fff;border:1px solid #dbeafe;border-radius:6px;padding:4px 8px;font-size:13px">{{serve_cmd}}</code>
</div>
<h2>{{t_types}}</h2><table><tr><th>{{t_type}}</th><th class="n">{{t_downloaded}}</th><th class="n">{{t_total}}</th><th class="n">{{t_share}}</th></tr>{{rows}}</table>
<h2>{{t_fresh}}</h2><table>
<tr><td>{{t_snap_range}}</td><td class="n">{{snap_range}}</td></tr>
<tr><td>{{t_old}}</td><td class="n">{{old_files}}</td></tr>
<tr><td>{{t_fallbacks}}</td><td class="n">{{fallbacks}}</td></tr>
<tr><td>{{t_no_snapshot}}</td><td class="n">{{no_snapshot}}</td></tr>
<tr><td>{{t_collisions}}</td><td class="n">{{collisions}}</td></tr>
<tr><td>{{t_rescued}}</td><td class="n">{{rescued}}</td></tr>
<tr><td>{{t_shared}}</td><td class="n">{{shared}}</td></tr>
<tr><td>{{t_left}}</td><td class="n">{{left}}</td></tr>
<tr><td>{{t_archive}}</td><td class="n">{{archive}}</td></tr>
<tr><td>{{t_unreadable}}</td><td class="n">{{unreadable}}</td></tr></table>

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
<h2>{{t_integrity}}</h2><table>
<tr><td>{{t_mirror}}</td><td class="n">{{mirror_files}}</td></tr>
<tr><td>{{t_unique}}</td><td class="n">{{unique_paths}}</td></tr>
<tr><td>{{t_foreign}}</td><td class="n">{{foreign}}</td></tr>
<tr><td>{{t_refs}}</td><td class="n">{{refs}}</td></tr>
<tr><td>{{t_broken}} <span style="color:#6b7280">{{t_broken_note}}</span></td><td class="n">{{broken}}</td></tr>
{{encodings}}
<tr><td>{{t_404}}</td><td class="n">{{missing}}</td></tr>
<tr><td>{{t_gunzipped}}</td><td class="n">{{gunzipped}}</td></tr>
<tr><td>{{t_mismatched}}</td><td class="n">{{mismatched}}</td></tr>
<tr><td>{{t_minifier}}</td><td class="n">{{minifier}}</td></tr>
<tr><td>{{t_errors}}</td><td class="n">{{errors}}</td></tr>
<tr><td>{{t_build}}</td><td>{{build}}</td></tr></table>
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
        assert_eq!(rows[0].field(body, 0).unwrap(), "20020120142510");
        assert_eq!(rows[0].field(body, 1).unwrap(), "http://example.com:80/");
        assert_eq!(rows[1].field(body, 1).unwrap(), "https://example.com/a");
        // Поле берётся из тела, а не из копии: для обычного адреса копии нет.
        assert!(matches!(
            rows[1].field(body, 1).unwrap(),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    /// Прежний разбор: `Vec<char>` и строка на каждое поле. Оставлен только в
    /// тестах — как эталон для сверки и как база для замера (Д-64, ФТ-158).
    fn parse_rows_legacy(body: &[u8]) -> Vec<Vec<String>> {
        let text = String::from_utf8_lossy(body);
        let chars: Vec<char> = text.chars().collect();
        let mut rows = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] != '[' {
                i += 1;
                continue;
            }
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

    /// Тело ответа CDX на `rows` строк — как приходит от архива.
    fn cdx_body(rows: usize) -> Vec<u8> {
        let mut text = String::with_capacity(rows * 220 + 8);
        text.push('[');
        for n in 0..rows {
            if n > 0 {
                text.push(',');
            }
            text.push_str(&format!(
                "[\"20240101120000\",\"https://example.com/catalog/product-{n}.html\",\"text/html\",\"200\",\"12345\",\"ABC{n}DEF\"]"
            ));
        }
        text.push(']');
        text.into_bytes()
    }

    /// Д-64: новый разбор даёт то же, что прежний, на обычном теле,
    /// на экранировании и на обрезанном ответе.
    #[test]
    fn parse_rows_matches_legacy() {
        let cases: Vec<Vec<u8>> = vec![
            cdx_body(200),
            br#"[["timestamp","original","mimetype"],["1","https://e.com/a","text/html"]]"#
                .to_vec(),
            br#"[["1","https://e.com/a\"b","text/html","200"]]"#.to_vec(),
            // обрыв тела: прежний разбор отдавал обрезанный адрес, новый — нет
            br#"[["1","https://e.co"#.to_vec(),
        ];
        // Тела, на которых оба разбора обязаны дать непустой результат (Д-92:
        // раньше пустой разбор сходился с прежним, и тест проходил на сломанном коде).
        for body in cases.iter().take(3) {
            let legacy = parse_rows_legacy(body);
            let fresh = parse_rows(body);
            assert!(
                !fresh.is_empty(),
                "разбор вернул пусто на теле {}",
                body.len()
            );
            assert_eq!(legacy.len(), fresh.len(), "число строк");
            for (old_row, new_row) in legacy.iter().zip(fresh.iter()) {
                assert_eq!(old_row.len(), new_row.len(), "число полей");
                for (idx, expected) in old_row.iter().enumerate().take(MAX_FIELDS) {
                    assert_eq!(
                        Some(expected.as_str()),
                        new_row.field(body, idx).as_deref(),
                        "поле {idx}"
                    );
                }
            }
        }
        // Обрыв тела — единственное ожидаемое расхождение: новый разбор-strict
        // отбрасывает недописанную строку, прежний отдавал обрезанный адрес.
        let broken = cases.last().expect("тело с обрывом");
        assert!(
            parse_rows(broken).is_empty(),
            "обрезанный адрес не должен сохраняться"
        );
    }

    /// ФТ-158: разбор по срезам не медленнее прежнего в пять раз. Замер
    /// помечен `#[ignore]`, потому что время зависит от машины; запуск:
    /// `cargo test --release -- --ignored bench_parse_rows --nocapture`.
    #[test]
    #[ignore = "замер производительности, запускается отдельно"]
    fn bench_parse_rows() {
        let body = cdx_body(20_000);
        let mib = body.len() as f64 / 1_048_576.0;
        // Прогрев: первый прогон включает страницы памяти.
        let _ = parse_rows(&body);
        let _ = parse_rows_legacy(&body);

        let mut legacy_best = f64::MAX;
        let mut fresh_best = f64::MAX;
        for _ in 0..5 {
            let started = std::time::Instant::now();
            let legacy = parse_rows_legacy(&body);
            legacy_best = legacy_best.min(started.elapsed().as_secs_f64());
            assert_eq!(legacy.len(), 20_000);
            let started = std::time::Instant::now();
            let fresh = parse_rows(&body);
            fresh_best = fresh_best.min(started.elapsed().as_secs_f64());
            assert_eq!(fresh.len(), 20_000);
        }
        println!(
            "parse_rows: тело {mib:.2} МБ, 20 000 строк — прежний {:.1} мс, новый {:.1} мс, ускорение {:.1}x",
            legacy_best * 1000.0,
            fresh_best * 1000.0,
            legacy_best / fresh_best
        );
        // Д-92: при нулевом знаменателе отношение давало бесконечность и тест
        // проходил на любом результате — считаем только по положительным замерам.
        assert!(legacy_best > 0.0 && fresh_best > 0.0, "замер не состоялся");
        assert!(
            legacy_best / fresh_best >= 5.0,
            "разбор по срезам обязан быть не медленнее чем в пять раз: {:.1}x",
            legacy_best / fresh_best
        );
    }

    /// Дата снимка с многобайтовыми символами не должна ронять прогон: раньше
    /// срез по байтам паниковал, а при `panic = "abort"` это конец всего прогона.
    #[test]
    fn stamp_readable_does_not_panic_on_multibyte() {
        assert_eq!(stamp_readable("20251115030405"), "2025-11-15");
        // Три трёхбайтовых символа: проверка длины проходит, срез по байтам — нет.
        let wide = "\u{4e2d}\u{4e2d}\u{4e2d}";
        assert_eq!(
            stamp_readable(wide),
            wide,
            "не-ASCII дата остаётся как есть"
        );
        assert_eq!(stamp_readable(""), "");
        assert_eq!(stamp_readable("2025"), "2025");
    }

    /// Конец списка целей ищется с учётом кавычек: `]` внутри пути — часть пути,
    /// а не конец списка (прежний поиск первого `]` оставлял пустой список).
    #[test]
    fn journal_targets_with_bracket_are_not_truncated() {
        let line =
            r#"{"path":"a.html","size":1,"mtime":2,"page":true,"targets":["a[1].html","b.html"]}"#;
        let (path, entry) = parse_journal_line(line).expect("строка разбирается");
        assert_eq!(path, "a.html");
        assert_eq!(
            entry.targets.as_slice(),
            ["a[1].html".to_string(), "b.html".to_string()]
        );

        let list = r#"["a[1].html","b.html"]"#;
        assert_eq!(
            targets_end(list),
            Some(list.len() - 1),
            "конец списка — последняя скобка, а не скобка внутри пути"
        );
    }

    /// Ключ ссылки не должен указывать за пределы копии: иначе добор писал бы не
    /// туда, куда собирается копия. Проверяется сам инвариант, а не форма записи:
    /// на Windows выход через «..» даёт путь с буквой диска, на Linux — обычный
    /// относительный, и это законно, пока путь остаётся внутри зеркала.
    #[test]
    fn ref_key_never_leads_outside_copy() {
        let dir = std::env::temp_dir().join(format!(
            "sr-key-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::create_dir_all(&dir);
        let engine = Engine::new("example.com", &dir, 1, 0).unwrap();
        let mirror = dir.join("mirror");
        assert_eq!(engine.ref_key("/a.html", "").as_deref(), Some("a.html"));
        for raw in [
            "../..",
            "../../etc/passwd",
            "C:/Windows/Temp/x.html",
            "//evil/x",
        ] {
            if let Some(key) = engine.ref_key(raw, "") {
                let dest = mirror.join(&key);
                assert!(
                    dest.starts_with(&mirror),
                    "ключ {raw} → {key} выводит запись за пределы копии"
                );
            }
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// Сетевой путь требует формы \\?\UNC\server\share: иначе из него получалось \\?\\\server и операции молча не работали.
    #[cfg(windows)]
    #[test]
    fn long_path_keeps_unc_share() {
        let unc = Path::new(r"\\srv\share\deep\file.html");
        assert_eq!(
            long_path(unc),
            PathBuf::from(r"\\?\UNC\srv\share\deep\file.html")
        );
        let plain = Path::new(r"C:\tmp\file.html");
        assert_eq!(long_path(plain), PathBuf::from(r"\\?\C:\tmp\file.html"));
        // Уже с префиксом — не трогаем.
        let ready = Path::new(r"\\?\C:\tmp\file.html");
        assert_eq!(long_path(ready), ready.to_path_buf());
    }

    /// `..` выше корня не снимает сам корень (найдено на код-ревью).
    ///
    /// Проверка про букву диска, поэтому только Windows: на Linux `C:\..\x`
    /// — обычное относительное имя, и ожидание `C:\x` там бессмысленно.
    #[cfg(windows)]
    #[test]
    fn parent_dir_does_not_remove_root() {
        let root_case = absolute_dir(Path::new(r"C:\..\x"));
        let text = root_case.to_string_lossy().to_lowercase();
        assert!(
            text.ends_with(r"c:\x"),
            "ожидался путь на диске C, получено {text}"
        );
        assert!(!text.contains("c:x"), "путь стал относительным: {text}");
        // Обычный `..` внутри дерева по-прежнему работает.
        let inner = absolute_dir(Path::new(r"C:\dev\a\..\b"))
            .to_string_lossy()
            .to_lowercase();
        assert!(inner.ends_with(r"dev\b"), "получено {inner}");
    }

    /// Понятная причина сетевой ошибки и код в скобках (визуальный тест 14.11.2026).
    #[test]
    fn net_errors_get_plain_reason() {
        let refused = explain_net(
            "io: Подключение не установлено, т.к. конечный компьютер отверг запрос на подключение. (os error 10061)",
        );
        assert!(refused.contains("10061"), "код потерян: {refused}");
        assert!(
            !refused.contains("конечный компьютер"),
            "системный текст остался: {refused}"
        );
        let slow = explain_net("timeout: per read");
        assert!(
            slow.to_lowercase().contains("время") || slow.contains("time"),
            "{slow}"
        );
        // Неизвестная ошибка остаётся как есть — врать про причину нельзя.
        assert_eq!(explain_net("http status 404"), "http status 404");
    }

    /// Путь для показа не содержит служебного префикса (Д-105).
    #[test]
    fn show_path_strips_long_prefix() {
        assert_eq!(show_path(Path::new(r"\\?\C:\tmp\a.html")), r"C:\tmp\a.html");
        assert_eq!(
            show_path(Path::new(r"\\?\UNC\srv\share\x")),
            r"\\srv\share\x"
        );
        assert_eq!(show_path(Path::new(r"C:\tmp")), r"C:\tmp");
    }

    /// Закрытие очереди будит ждущего воркера: иначе `join` в конце прогона висит.
    #[test]
    fn queue_close_wakes_waiter() {
        use std::sync::mpsc;
        let queue = std::sync::Arc::new(Queue::new(Vec::new(), 0));
        let (tx, rx) = mpsc::channel();
        let waiter = {
            let queue = std::sync::Arc::clone(&queue);
            std::thread::spawn(move || {
                let taken = queue.pop();
                let _ = tx.send(taken.is_none());
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        queue.close();
        let woken = rx.recv_timeout(std::time::Duration::from_secs(2));
        assert_eq!(
            woken,
            Ok(true),
            "ждущий воркер обязан проснуться по закрытию"
        );
        waiter.join().unwrap();
    }

    /// Запрос `fl=timestamp` отдаёт по одному полю в строке: такие строки
    /// обязаны разбираться, иначе добор дат снимков молча не работает.
    #[test]
    fn parse_rows_keeps_single_field_rows() {
        let body = "[[\"timestamp\"],\n[\"20261112010903\"],\n[\"20261112021434\"]]";
        let rows = parse_rows(body.as_bytes());
        assert_eq!(rows.len(), 2, "шапка отброшена, две даты остались");
        assert_eq!(rows[0].field(body.as_bytes(), 0).unwrap(), "20261112010903");
        assert_eq!(rows[1].field(body.as_bytes(), 0).unwrap(), "20261112021434");
    }

    /// Д-64: экранирование, обрыв тела и лишние поля.
    #[test]
    fn parse_rows_survives_escapes_and_broken_body() {
        let body = br#"[["1","https://e.com/a\"b","text/html","200"]]"#;
        let rows = parse_rows(body);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].field(body, 1).unwrap(), "https://e.com/a\"b");

        // Тело обрывается на середине поля — строки нет, но и паники нет.
        let broken = br#"[["1","https://e.co"#;
        assert!(parse_rows(broken).is_empty(), "незакрытая строка не адрес");

        // Полей больше шести: первые шесть читаются, счётчик сохраняется.
        let long = "[[\"1\",\"https://e.com/\",\"text/html\",\"200\",\"10\",\"D\",\"лишнее\"]]";
        let rows = parse_rows(long.as_bytes());
        assert_eq!(rows[0].len(), 7, "счётчик полей сохраняется");
        assert_eq!(rows[0].field(long.as_bytes(), 5).unwrap(), "D");
        assert!(
            rows[0].field(long.as_bytes(), 6).is_none(),
            "седьмое поле не хранится"
        );
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

    /// Д-55 часть 2: после добора пересобираются только затронутые страницы.
    /// Ссылка, оставленная абсолютной (цели не было), должна стать относительной,
    /// а нетронутая страница — сохранить прежнее время изменения.
    #[test]
    fn dirty_build_rewrites_only_affected_pages() {
        let dir = std::env::temp_dir().join(format!(
            "sr-dirty-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::create_dir_all(dir.join("mirror"));
        let engine = Engine::new("example.com", &dir, 1, 0).unwrap();
        let put = |rel: &str, body: &str| {
            let full = dir.join("mirror").join(rel);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, body).unwrap();
        };
        put(
            "index.html",
            "<html><body><a href=\"https://example.com/a.html\">a</a>             <a href=\"https://example.com/missing.html\">m</a></body></html>",
        );
        put("a.html", "<html><body>a</body></html>");
        engine.build().unwrap();
        let page = dir.join("site").join("index.html");
        let first = fs::read_to_string(&page).unwrap();
        assert!(
            first.contains("https://example.com/missing.html"),
            "цели нет — адрес остался абсолютным: {first}"
        );

        // Имитируем добор: файл появился, страница стала «грязной».
        put("missing.html", "<html><body>m</body></html>");
        let mut dirty = HashSet::new();
        dirty.insert("index.html".to_string());
        *engine.mirror_cache.lock().unwrap() = None;
        engine.build_dirty(Some(&dirty)).unwrap();
        let second = fs::read_to_string(&page).unwrap();
        assert!(
            second.contains("href=\"missing.html\""),
            "после пересборки ссылка относительная: {second}"
        );
        let _ = fs::remove_dir_all(&dir);
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
        // Строки собираются тем же разбором, что и в работе: тест проверяет
        // реальный путь «тело → строка → карта», а не отдельную подделку.
        let fold = |best: &mut HashMap<String, (String, String, String, u64, String)>,
                    ts: &str,
                    url: &str,
                    mime: &str,
                    from_window: bool| {
            let body = format!("[[\"{ts}\",\"{url}\",\"{mime}\",\"200\",\"10\"]]");
            let rows = parse_rows(body.as_bytes());
            assert_eq!(rows.len(), 1);
            fold_row(best, "example.com", &rows[0], body.as_bytes(), from_window);
        };
        // Старый снимок, затем новый того же адреса.
        fold(
            &mut best,
            "20200101000000",
            "https://example.com/a",
            "text/html",
            true,
        );
        fold(
            &mut best,
            "20240101000000",
            "https://example.com/a",
            "text/html",
            true,
        );
        assert_eq!(best.len(), 1, "один адрес — одна запись");
        assert_eq!(best["https://example.com/a"].0, "20240101000000");
        // Более старый снимок не должен перебить уже найденный свежий.
        fold(
            &mut best,
            "20190101000000",
            "https://example.com/a",
            "text/html",
            true,
        );
        assert_eq!(best["https://example.com/a"].0, "20240101000000");
        // Добор из полной истории не трогает страницу, взятую из окна.
        fold(
            &mut best,
            "20250101000000",
            "https://example.com/a",
            "text/html",
            false,
        );
        assert_eq!(
            best["https://example.com/a"].0, "20240101000000",
            "страница из окна не заменяется добором"
        );
        // А ресурс — заменяется: для картинок окно не действует.
        fold(
            &mut best,
            "20200101000000",
            "https://example.com/i.png",
            "image/png",
            true,
        );
        fold(
            &mut best,
            "20250101000000",
            "https://example.com/i.png",
            "image/png",
            false,
        );
        assert_eq!(best["https://example.com/i.png"].0, "20250101000000");
        // Короткая строка игнорируется, а не паникует.
        let short = Row {
            spans: [Span::default(); MAX_FIELDS],
            count: 1,
        };
        fold_row(&mut best, "example.com", &short, b"", true);
        assert_eq!(best.len(), 2);
    }

    /// Добор по ссылкам: ключ считается известным и как файл, и как каталог.
    /// Буфер переиспользуется — в этом и смысл правки (без `format!` в цикле).
    #[test]
    fn known_key_matches_file_and_directory_index() {
        let mut known: HashSet<String> = HashSet::new();
        known.insert("index.html".to_string());
        known.insert("a/index.html".to_string());
        let mut probe = String::new();
        assert!(
            Engine::known_key(&known, "index.html", &mut probe),
            "файл в корне"
        );
        assert!(
            Engine::known_key(&known, "a", &mut probe),
            "каталог с index.html"
        );
        assert!(
            !Engine::known_key(&known, "b", &mut probe),
            "неизвестный ключ"
        );
        assert_eq!(probe, "b/index.html", "буфер содержит последний запрос");
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
        assert_eq!(rows[0].field(body, 1).unwrap(), "https://e.com/a\"b");
        // \u0410 — буква «А»: escape этого вида тоже разбирается.
        let uni = br#"[["2001","https://e.com/\u0410","text/html","200"]]"#;
        let rows = parse_rows(uni);
        assert_eq!(rows[0].field(uni, 1).unwrap(), "https://e.com/А");
        // Пара суррогатов собирается в один символ: эмодзи в адресе не рвётся.
        let pair = br#"[["2001","https://e.com/\uD83D\uDE00","text/html","200"]]"#;
        let rows = parse_rows(pair);
        assert_eq!(rows[0].field(pair, 1).unwrap(), "https://e.com/\u{1F600}");
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

    /// Отметка проверки двигается только тогда, когда файл в этом прогоне
    /// спрашивали у архива: иначе либо срок перепроверки не наступает никогда,
    /// либо весь объём зеркала качается заново каждый прогон.
    #[test]
    fn checked_mark_moves_only_after_recheck() {
        let dir = workdir("checked");
        put(
            &dir,
            "index.html",
            "<html><body>привет</body></html>".as_bytes(),
        );
        let engine = Engine::new("example.com", &dir, 1, 0).unwrap();
        engine.build().unwrap();

        let journal_file = dir.join("data").join("build.jsonl");
        let zero = |text: &str| -> String {
            text.lines()
                .map(|line| match line.find("\"checked\":") {
                    Some(at) => {
                        let head = &line[..at + "\"checked\":".len()];
                        let tail = line[at + "\"checked\":".len()..]
                            .split_once(',')
                            .map(|(_, rest)| format!(",{rest}"))
                            .unwrap_or_default();
                        format!("{head}0{tail}")
                    }
                    None => line.to_string(),
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        // 1. Пропуск без перепроверки: отметка не двигается (срок ещё не наступил).
        let first = fs::read_to_string(&journal_file).unwrap();
        fs::write(&journal_file, format!("{}\n", zero(&first))).unwrap();
        engine.build().unwrap();
        let second = fs::read_to_string(&journal_file).unwrap();
        assert!(
            second.contains("\"checked\":0,"),
            "без перепроверки отметка остаётся прежней: {second}"
        );

        // 2. Файл попал в набор перепроверки: отметка обновляется.
        if let Ok(mut set) = engine.shared.refresh.lock() {
            set.insert("index.html".to_string());
        }
        engine.build().unwrap();
        let third = fs::read_to_string(&journal_file).unwrap();
        assert!(
            !third.contains("\"checked\":0,"),
            "после перепроверки отметка обновлена: {third}"
        );
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

    /// 14.11.2026: схему меняем только при отказе соединения, а не при медленном ответе.
    #[test]
    fn scheme_switch_only_when_unreachable() {
        for slow in [
            "timeout: per read",
            "timeout: connect",
            "timed out waiting for body",
            "body: connection closed while reading",
        ] {
            assert!(!scheme_switch_worthwhile(slow), "медленный ответ: {slow}");
        }
        for dead in [
            "connect: connection refused",
            "connect: network unreachable",
            "dns: no address",
        ] {
            assert!(scheme_switch_worthwhile(dead), "адрес недоступен: {dead}");
        }
        // Д-102: ошибка TLS/сертификата не переводит прогон на открытый http://.
        assert!(!scheme_switch_worthwhile("tls: handshake failure"));
    }

    /// 14.11.2026: медленный ответ https не уводит прогон на http.
    #[test]
    fn slow_answer_keeps_https() {
        let root = workdir("scheme-slow");
        let engine = Engine::new("example.com", &root, 1, 0).unwrap();
        engine
            .shared
            .archive_failed("список недоступен", "timeout: per read");
        assert_eq!(engine.shared.archive_scheme(), "https");
        let _ = fs::remove_dir_all(&root);
    }

    /// 14.11.2026: если и запасная схема не отвечает, программа возвращается на https.
    #[test]
    fn fallback_scheme_gives_up_after_three_failures() {
        let root = workdir("scheme-back");
        let engine = Engine::new("example.com", &root, 1, 0).unwrap();
        engine.shared.archive_failed("сеть", "connect: refused");
        assert_eq!(engine.shared.archive_scheme(), "http");
        for _ in 0..ARCHIVE_ALT_TRIES {
            engine.shared.archive_failed("сеть", "connect: refused");
        }
        assert_eq!(engine.shared.archive_scheme(), "https", "возврат на https");
        let _ = fs::remove_dir_all(&root);
    }

    /// ОШ-2 + Д-97: схема переключается один раз и держится до конца прогона.
    #[test]
    fn archive_scheme_sticks_after_first_switch() {
        let root = workdir("scheme");
        let engine = Engine::new("example.com", &root, 1, 0).unwrap();
        assert_eq!(engine.shared.archive_base(), "https://web.archive.org");
        assert_eq!(engine.shared.archive_scheme(), "https");
        engine.shared.switch_archive("тест");
        assert_eq!(engine.shared.archive_base(), "http://web.archive.org");
        assert_eq!(engine.shared.archive_scheme(), "http");
        // Две неудачи подряд не возвращают https: переключение липкое.
        engine.shared.switch_archive("тест");
        engine.shared.switch_archive("тест");
        assert_eq!(engine.shared.archive_base(), "http://web.archive.org");
        // Новый прогон снова начинает с https — «http» не залипает между запусками.
        let next = Engine::new("example.com", &root, 1, 0).unwrap();
        assert_eq!(next.shared.archive_scheme(), "https");
        let _ = fs::remove_dir_all(&root);
    }

    /// Д-98: UA содержит версию изделия, а не литерал.
    #[test]
    fn user_agent_carries_version() {
        assert!(UA.starts_with("SiteRestorer/"), "UA: {UA}");
        assert!(
            UA.contains(crate::APP_VERSION),
            "в UA нет версии {}. UA: {UA}",
            crate::APP_VERSION
        );
        assert!(UA.contains("contact:"), "в UA нет контакта: {UA}");
    }

    /// Д-99: `Retry-After` числом, HTTP-датой и мусором.
    #[test]
    fn retry_after_parses_number_and_http_date() {
        assert_eq!(retry_after_secs("120"), Some(120.0));
        assert_eq!(retry_after_secs(" 7 "), Some(7.0));
        // Потолок и защита от бесконечности.
        assert_eq!(retry_after_secs("100000"), Some(MAX_RETRY_WAIT));
        assert_eq!(retry_after_secs("inf"), None);
        // HTTP-дата через минуту: пауза около минуты, а не ноль.
        let stamp = stamp_from_secs(now_secs() + 60);
        let date = format!(
            "Tue, {} {} {} {}:{}:{} GMT",
            &stamp[6..8],
            [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"
            ][stamp[4..6].parse::<usize>().unwrap() - 1],
            &stamp[0..4],
            &stamp[8..10],
            &stamp[10..12],
            &stamp[12..14]
        );
        let wait = retry_after_secs(&date).expect("дата должна разбираться");
        assert!(
            (55.0..=65.0).contains(&wait),
            "ожидалось около 60 с, получено {wait} по {date}"
        );
        // Прошлое — ждать нечего.
        assert_eq!(retry_after_secs("Tue, 10 Nov 1999 17:07:55 GMT"), Some(0.0));
        // Мусор и пустота.
        assert_eq!(retry_after_secs("не дата"), None);
        assert_eq!(retry_after_secs(""), None);
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
        // 2026-11-10T17:07:55Z — точное значение, а не только год (Д-92).
        let stamp = stamp_from_secs(1_794_330_475);
        assert_eq!(stamp, "20261110170755", "метка времени целиком");
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
        let site_root = fs::canonicalize(root.join("site")).expect("каталог копии");
        let (files, _unreadable) = walk_files(&site_root);
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

    /// П-Д2: inline-<script>, <style> и HTML-комментарии не портятся шаблоном атрибутов.
    #[test]
    fn inline_script_is_not_corrupted_by_link_rewriter() {
        let root = workdir("inlinejs");
        let page = concat!(
            "<html><head><style>body{background:url(/img/bg.png)}</style></head><body>",
            "<!-- <a href=\"/commented.html\">c</a> -->",
            "<a href=\"/ok.html\">ok</a>",
            "<script src=\"https://example.com/js/app.js\">",
            "k.async = 1, k.src = r, a.parentNode.insertBefore(k, a);",
            "el.async = true;el.src = (document.location.protocol == 'https:' ? 'https:' : 'http:') + '//a.ru/b.js';",
            "location.href = match[1];",
            "var s = \"<a href=\\'/bitrix/x\\'>\";",
            "var local = \"https://example.com/js/app.js\";",
            "var api = \"https://example.com/api/v1\";",
            "</script></body></html>"
        );
        put(&root, "index.html", page.as_bytes());
        put(&root, "ok.html", b"<html><body>ok</body></html>");
        put(&root, "img/bg.png", b"\x89PNG\r\n\x1a\n");
        put(&root, "js/app.js", b"console.log(1);");
        let engine = build(&root);
        let stats = engine.verify();
        let html = read(&root, "index.html");
        assert!(html.contains("k.src = r, a"), "JS испорчен: {html}");
        assert!(
            html.contains("el.src = (document.location.protocol"),
            "JS испорчен: {html}"
        );
        assert!(
            html.contains("location.href = match[1]"),
            "location.href испорчен: {html}"
        );
        assert!(
            html.contains("<a href=\\'/bitrix/x\\'>"),
            "строка JS испорчена: {html}"
        );
        assert!(
            html.contains("src=\"js/app.js\""),
            "src скрипта не переписан: {html}"
        );
        assert!(
            html.contains("var local = \"js/app.js\""),
            "локальный URL в <script> не переписан: {html}"
        );
        assert!(
            html.contains("var api = \"https://example.com/api/v1\""),
            "API URL изменён: {html}"
        );
        assert!(
            html.contains("url(img/bg.png)"),
            "style url() не переписан: {html}"
        );
        assert!(
            html.contains("<!-- <a href=\"/commented.html\">c</a> -->"),
            "комментарий тронут: {html}"
        );
        assert_eq!(stats.broken, 0, "комментарий не должен давать битую ссылку");
        let _ = fs::remove_dir_all(&root);
    }

    /// Р3-2 + ФТ-61 + ФТ-147: .js-файл переписывает цели в копии, а API оставляет абсолютным.
    #[test]
    fn js_file_rewrites_local_and_keeps_api() {
        let root = workdir("jsrewrite");
        put(
            &root,
            "js/app.js",
            br#"const img = "https://example.com/img/a.png"; const api = "https://example.com/api/v1"; const ext = "https://notexample.com/img/a.png";"#,
        );
        put(&root, "img/a.png", b"\x89PNG\r\n\x1a\n");
        let engine = build(&root);
        let js = read(&root, "js/app.js");
        assert!(
            js.contains("const img = \"../img/a.png\""),
            "JS не переписан: {js}"
        );
        assert!(
            js.contains("const api = \"https://example.com/api/v1\""),
            "API изменён: {js}"
        );
        assert!(
            js.contains("const ext = \"https://notexample.com/img/a.png\""),
            "чужой домен тронут: {js}"
        );
        assert_eq!(
            engine.shared.counters.script_left.load(Ordering::Relaxed),
            1
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Р3-1: ответ 429/504 с `Content-Encoding: gzip` и пустым/битым телом возвращает статус, а не Err.
    #[test]
    fn gzip_error_status_keeps_status_and_retry_after() {
        let root = workdir("gziperr");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = std::thread::spawn(move || {
            for idx in 0..2 {
                let (mut sock, _) = listener.accept().unwrap();
                let mut buf = [0u8; 1024];
                let _ = std::io::Read::read(&mut sock, &mut buf);
                let resp = if idx == 0 {
                    b"HTTP/1.1 504 Gateway Timeout\r\nContent-Encoding: gzip\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_slice()
                } else {
                    b"HTTP/1.1 429 Too Many Requests\r\nContent-Encoding: gzip\r\nRetry-After: 3\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot-gzip!".as_slice()
                };
                let _ = std::io::Write::write_all(&mut sock, resp);
            }
        });
        let engine = Engine::new("example.com", &root, 1, 0).unwrap();
        let f1 = engine
            .shared
            .fetch_stream(&format!("http://{addr}/cdx?1"), &mut |_, _| {})
            .expect("504 не должен падать на gzip");
        assert_eq!(f1.status, 504);
        let f2 = engine
            .shared
            .fetch_stream(&format!("http://{addr}/cdx?2"), &mut |_, _| {})
            .expect("429 не должен падать на битом gzip");
        assert_eq!(f2.status, 429);
        assert_eq!(f2.retry_after, Some(3.0));
        let _ = srv.join();
        let _ = fs::remove_dir_all(&root);
    }

    /// П-Д1: если showNumPages > 1, inventory обходит все страницы CDX, не веря quick_query.
    #[test]
    fn inventory_walks_all_pages_when_numpages_gt_one() {
        let root = workdir("invpages");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = std::thread::spawn(move || {
            for _ in 0..4 {
                let (mut sock, _) = listener.accept().unwrap();
                let mut buf = [0u8; 2048];
                let n = std::io::Read::read(&mut sock, &mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let body = if req.contains("showNumPages=true") {
                    "[[\"numpages\"],[\"3\"]]".to_string()
                } else if req.contains("page=0") {
                    "[[\"timestamp\",\"original\",\"mimetype\",\"statuscode\",\"length\",\"digest\"],[\"20260101000000\",\"https://example.com/\",\"text/html\",\"200\",\"100\",\"A\"],[\"20260101000000\",\"https://example.com/p1\",\"text/html\",\"200\",\"100\",\"B\"]]".to_string()
                } else if req.contains("page=1") {
                    "[[\"timestamp\",\"original\",\"mimetype\",\"statuscode\",\"length\",\"digest\"],[\"20260101000000\",\"https://example.com/p2\",\"text/html\",\"200\",\"100\",\"C\"],[\"20260101000000\",\"https://example.com/p3\",\"text/html\",\"200\",\"100\",\"D\"]]".to_string()
                } else if req.contains("page=2") {
                    "[[\"timestamp\",\"original\",\"mimetype\",\"statuscode\",\"length\",\"digest\"],[\"20260101000000\",\"https://example.com/p4\",\"text/html\",\"200\",\"100\",\"E\"]]".to_string()
                } else {
                    "[]".to_string()
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = std::io::Write::write_all(&mut sock, head.as_bytes());
                let _ = std::io::Write::write_all(&mut sock, body.as_bytes());
            }
        });
        let mut engine = Engine::new("example.com", &root, 1, 0).unwrap();
        Arc::get_mut(&mut engine.shared).unwrap().agent_archive = format!("http://{addr}");
        engine.inventory().expect("inventory");
        assert_eq!(
            engine.items.len(),
            5,
            "должны собраться все 5 адресов с 3 страниц"
        );
        assert_eq!(engine.index_pages, 3);
        assert!(!engine.index_truncated);
        let _ = srv.join();
        let _ = fs::remove_dir_all(&root);
    }

    /// П-Д1: если showNumPages недоступен (400/503), проверяется page=1 и при наличии строк запускается полный обход.
    #[test]
    fn inventory_checks_page_one_when_estimate_unavailable() {
        let root = workdir("invnoest");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = std::thread::spawn(move || {
            for _ in 0..6 {
                let (mut sock, _) = listener.accept().unwrap();
                let mut buf = [0u8; 2048];
                let n = std::io::Read::read(&mut sock, &mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let (status, body) = if req.contains("showNumPages=true") {
                    ("400 Bad Request", "error".to_string())
                } else if req.contains("page=0") {
                    ("200 OK", "[[\"timestamp\",\"original\",\"mimetype\",\"statuscode\",\"length\",\"digest\"],[\"20260101000000\",\"https://example.com/\",\"text/html\",\"200\",\"100\",\"A\"]]".to_string())
                } else if req.contains("page=1") {
                    ("200 OK", "[[\"timestamp\",\"original\",\"mimetype\",\"statuscode\",\"length\",\"digest\"],[\"20260101000000\",\"https://example.com/p2\",\"text/html\",\"200\",\"100\",\"B\"]]".to_string())
                } else if req.contains("page=2") {
                    ("200 OK", "[[\"timestamp\",\"original\",\"mimetype\",\"statuscode\",\"length\",\"digest\"]]".to_string())
                } else {
                    ("200 OK", "[[\"timestamp\",\"original\",\"mimetype\",\"statuscode\",\"length\",\"digest\"],[\"20260101000000\",\"https://example.com/\",\"text/html\",\"200\",\"100\",\"A\"]]".to_string())
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = std::io::Write::write_all(&mut sock, head.as_bytes());
                let _ = std::io::Write::write_all(&mut sock, body.as_bytes());
            }
        });
        let mut engine = Engine::new("example.com", &root, 1, 0).unwrap();
        Arc::get_mut(&mut engine.shared).unwrap().agent_archive = format!("http://{addr}");
        engine.inventory().expect("inventory");
        assert_eq!(
            engine.items.len(),
            2,
            "должны собраться адреса и с page=0, и с page=1"
        );
        assert!(!engine.index_truncated);
        let _ = srv.join();
        let _ = fs::remove_dir_all(&root);
    }

    /// Р3-3: после удаления сирот в инкрементальной сборке site_cache, verify() и report.html согласованы.
    #[test]
    fn drop_orphans_updates_site_cache_for_verify_and_report() {
        let root = workdir("orphanscache");
        put(
            &root,
            "index.html",
            b"<html><body><a href=\"/a.html\">a</a></body></html>",
        );
        put(&root, "a.html", b"<html><body>a</body></html>");
        let first = build(&root);
        assert_eq!(first.verify().broken, 0);
        assert_eq!(first.site_files.load(Ordering::Relaxed), 2);

        fs::remove_file(root.join("mirror").join("a.html")).unwrap();
        let second = Engine::new("example.com", &root, 1, 0).unwrap();
        second.build().unwrap();
        let stats = second.verify();
        assert_eq!(
            stats.broken, 1,
            "удалённый сирота должен стать битой ссылкой"
        );
        assert_eq!(second.site_files.load(Ordering::Relaxed), 1);
        let report_path = second.report(stats).unwrap();
        let doc = fs::read_to_string(report_path).unwrap();
        assert!(
            doc.contains("<div class=\"v\">1</div>"),
            "в отчёте должен быть 1 файл: {doc}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Р3-4: Queue::mark_seen не пускает уже лежащие на диске пути обратно в очередь.
    #[test]
    fn queue_mark_seen_blocks_already_paths() {
        let item = |p: &str| Item {
            url: format!("https://example.com/{p}"),
            ts: "20260101000000".to_string(),
            orig: format!("https://example.com/{p}"),
            mime: "text/html".to_string(),
            path: p.to_string(),
            kind: "html",
            len: 0,
            digest: String::new(),
            same_as: String::new(),
        };
        let queue = Queue::new(vec![item("todo.html")], 1);
        queue.mark_seen(["already.html"]);
        assert!(
            !queue.push(item("already.html")),
            "already не должен добавляться повторно"
        );
        assert!(
            queue.push(item("fresh.html")),
            "новый путь должен добавляться"
        );
    }

    /// Д-102: ошибка TLS-сертификата не переводит прогон на открытый http://.
    #[test]
    fn tls_certificate_error_does_not_switch_scheme() {
        assert!(!scheme_switch_worthwhile(
            "tls handshake failed: invalid certificate"
        ));
        assert!(!scheme_switch_worthwhile("rustls: UnknownIssuer"));
        assert!(scheme_switch_worthwhile(
            "tcp connect error: connection refused (os error 10061)"
        ));
        assert!(explain_net("invalid certificate: UnknownIssuer").contains("сертификат"));
    }

    /// ФТ-142/143: текстовый файл больше text_limit копируется как есть без переписывания.
    #[test]
    fn text_limit_copies_large_file_as_is() {
        let root = workdir("textlimit");
        let raw = b"<html><body><a href=\"/x.html\">x</a></body></html>";
        put(&root, "index.html", raw);
        let mut engine = Engine::new("example.com", &root, 1, 0).unwrap();
        engine.set_text_limit(10);
        engine.build().unwrap();
        let built = fs::read(root.join("site").join("index.html")).unwrap();
        assert_eq!(
            built, raw,
            "большой текстовый файл должен копироваться байт-в-байт"
        );
        assert_eq!(engine.text_skipped.load(Ordering::Relaxed), 1);
        let _ = fs::remove_dir_all(&root);
    }

    /// П-Д4: пути непрочитанных каталогов сохраняются и попадают в report.html.
    #[test]
    fn unreadable_dir_paths_appear_in_report() {
        let root = workdir("unreadable_report");
        put(&root, "index.html", b"<html><body>ok</body></html>");
        let engine = build(&root);
        let missing_dir = root.join("missing_sub");
        let (_files, bad) = walk_files_detailed(&missing_dir);
        assert_eq!(bad.len(), 1);
        engine.note_unreadable(&missing_dir, &bad);
        let stats = engine.verify();
        let doc = fs::read_to_string(engine.report(stats).unwrap()).unwrap();
        assert!(
            doc.contains("missing_sub"),
            "путь непрочитанного каталога должен быть в отчёте: {doc}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Д-135: потолок Gate согласован с числом воркеров, слот освобождается при suspend.
    #[test]
    fn gate_respects_worker_cap() {
        let gate = Gate::new_capped(2, 1);
        assert_eq!(gate.limit(), 1);
        gate.set_limit(24);
        assert_eq!(gate.limit(), 1, "потолок 1 воркера не должен превышаться");

        let gate2 = Gate::new_capped(2, 6);
        assert_eq!(gate2.limit(), 2);
        gate2.set_limit(12);
        assert_eq!(gate2.limit(), 6);
    }

    /// Д-135: AIMD не снижает потоки на мелких файлах, сбрасывает 429-серию на чистом окне
    /// и реагирует на 5xx-перегрузку и резкий рост задержки.
    #[test]
    fn aimd_handles_small_files_streak_and_overload() {
        let mut streak = 2;
        let mut base = 700.0;
        // Чистое окно с мелкими файлами и нормальной задержкой: серия 429 сбрасывается,
        // а лимит растёт, а не падает.
        let (next, pause) = aimd_step(6, 16, &mut streak, &mut base, 5, 2000.0, 0, 0, 750.0);
        assert!(!pause);
        assert_eq!(streak, 0, "серия 429 обязана сбрасываться на чистом окне");
        assert_eq!(next, 7);

        // Три окна подряд с 429 вызывают паузу 120 с и сброс к 2.
        for i in 0..2 {
            let (l, p) = aimd_step(8, 16, &mut streak, &mut base, 0, 0.0, 1, 0, 0.0);
            assert!(!p, "итерация {i} ещё без паузы");
            assert_eq!(l, 4);
        }
        let (l, p) = aimd_step(4, 16, &mut streak, &mut base, 0, 0.0, 1, 0, 0.0);
        assert!(p);
        assert_eq!(l, 2);

        // Перегрузка 5xx/таймаутами мягко снижает лимит.
        let (l, p) = aimd_step(8, 16, &mut streak, &mut base, 0, 0.0, 0, 2, 0.0);
        assert!(!p);
        assert_eq!(l, 6);

        // Резкий рост задержки (>2.5x от базовой и >3 с) снижает лимит на 1.
        let (l, p) = aimd_step(8, 16, &mut streak, &mut base, 1, 1000.0, 0, 0, 4500.0);
        assert!(!p);
        assert_eq!(l, 7);
    }

    /// ОШ-15: cleanup_temp действительно находит и удаляет .part в mirror и data.
    #[test]
    fn cleanup_temp_removes_part_files() {
        let root = workdir("cleanpart");
        let m_sub = root.join("mirror").join("sub");
        let d_dir = root.join("data");
        fs::create_dir_all(&m_sub).unwrap();
        fs::create_dir_all(&d_dir).unwrap();
        let part1 = m_sub.join("a.css.part");
        let keep = m_sub.join("a.css");
        let part2 = d_dir.join("build.jsonl.part");
        fs::write(&part1, b"123").unwrap();
        fs::write(&keep, b"body{}").unwrap();
        fs::write(&part2, b"{}").unwrap();

        let engine = Engine::new("example.com", &root, 2, 0).unwrap();
        let removed = engine.cleanup_temp();
        assert_eq!(removed, 2, "должны удалиться оба .part-файла");
        assert!(!part1.exists());
        assert!(!part2.exists());
        assert!(keep.exists(), "готовый файл должен сохраниться");
        let _ = fs::remove_dir_all(&root);
    }

    /// ОШ-16: локализованный код отказа 10061 переключает схему https -> http.
    #[test]
    fn scheme_switch_handles_localized_10061() {
        assert!(scheme_switch_worthwhile(
            "io: Подключение не установлено, т.к. конечный компьютер отверг запрос на подключение. (os error 10061)"
        ));
        assert!(scheme_switch_worthwhile(
            "network unreachable (os error 10051)"
        ));
    }

    /// Д-137: обрыв тела внутри save_body повторяется второй попыткой и сохраняет файл.
    #[test]
    fn attempt_retries_when_save_body_drops_mid_stream() {
        let root = workdir("bodyretry");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = std::thread::spawn(move || {
            if let Ok((mut s1, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = std::io::Read::read(&mut s1, &mut buf);
                let head = "HTTP/1.1 200 OK\r\nContent-Length: 500\r\nConnection: close\r\n\r\n";
                let _ = std::io::Write::write_all(&mut s1, head.as_bytes());
                let _ = std::io::Write::write_all(&mut s1, b"<html><body>partial");
            }
            if let Ok((mut s2, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = std::io::Read::read(&mut s2, &mut buf);
                let body = format!("<html><body>{}</body></html>", "x".repeat(300));
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = std::io::Write::write_all(&mut s2, head.as_bytes());
                let _ = std::io::Write::write_all(&mut s2, body.as_bytes());
            }
        });
        let mut engine = Engine::new("example.com", &root, 2, 0).unwrap();
        Arc::get_mut(&mut engine.shared).unwrap().agent_archive = format!("http://{addr}");
        let item = Item {
            url: "https://example.com/page.html".to_string(),
            ts: "20260101000000".to_string(),
            orig: "https://example.com/page.html".to_string(),
            mime: "text/html".to_string(),
            path: "page.html".to_string(),
            kind: "html",
            len: 320,
            digest: String::new(),
            same_as: String::new(),
        };
        let res = engine.shared.attempt(&item, "20260101000000", 20);
        assert!(
            matches!(res, Attempt::Saved { .. }),
            "повтор после обрыва тела должен спасти файл"
        );
        assert!(root.join("mirror").join("page.html").exists());
        assert!(!root.join("mirror").join("page.html.part").exists());
        let _ = srv.join();
        let _ = fs::remove_dir_all(&root);
    }

    /// Д-136: добор по ссылкам сохраняет каталоги как `<dir>/index.html`, не мешая вложенным путям.
    #[test]
    fn rescue_maps_directory_keys_to_index_html() {
        let root = workdir("rescuedir");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = std::thread::spawn(move || {
            for _ in 0..2 {
                if let Ok((mut s, _)) = listener.accept() {
                    let mut buf = [0u8; 1024];
                    let _ = std::io::Read::read(&mut s, &mut buf);
                    let body = format!("<html><body>{}</body></html>", "a".repeat(300));
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = std::io::Write::write_all(&mut s, head.as_bytes());
                    let _ = std::io::Write::write_all(&mut s, body.as_bytes());
                }
            }
        });
        let mut engine = Engine::new("example.com", &root, 2, 0).unwrap();
        Arc::get_mut(&mut engine.shared).unwrap().agent_archive = format!("http://{addr}");
        let got = engine.rescue(&[
            "catalog/about".to_string(),
            "catalog/about/history.html".to_string(),
        ]);
        assert_eq!(got, 2);
        assert!(
            root.join("mirror")
                .join("catalog")
                .join("about")
                .join("index.html")
                .is_file()
        );
        assert!(
            root.join("mirror")
                .join("catalog")
                .join("about")
                .join("history.html")
                .is_file()
        );
        let _ = srv.join();
        let _ = fs::remove_dir_all(&root);
    }

    /// П-1 / Д-138: gzip-сжатый CSS без заголовка Content-Encoding распаковывается при сборке и в commit.
    #[test]
    fn gzipped_css_is_unpacked_automatically() {
        use std::io::Write as _;
        let root = workdir("gunzipcss");
        let css = b"body { color: #123456; margin: 0; }";
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(css).unwrap();
        let gz = enc.finish().unwrap();
        assert!(gz.starts_with(&[0x1F, 0x8B]));

        put(
            &root,
            "index.html",
            b"<html><head><link rel=\"stylesheet\" href=\"/a.css\"></head></html>",
        );
        put(&root, "a.css", &gz);
        let engine = build(&root);
        let built_css = fs::read(root.join("site").join("a.css")).unwrap();
        assert_eq!(built_css, css, "CSS в копии должен быть распакован");
        assert_eq!(engine.build_gunzipped.load(Ordering::Relaxed), 1);
        let _ = fs::remove_dir_all(&root);
    }

    /// П-3 / Д-139: корневые ссылки вне копии уходят на живой сайт, а verify/rescue всё ещё видят цель.
    #[test]
    fn missing_root_links_become_absolute_while_verify_counts_broken() {
        let root = workdir("rootmiss");
        put(
            &root,
            "index.html",
            b"<html><body><a href=\"/absent.html\">x</a></body></html>",
        );
        let engine = build(&root);
        let html = read(&root, "index.html");
        assert!(
            html.contains("href=\"https://example.com/absent.html\""),
            "корневая ссылка без цели должна стать абсолютной: {html}"
        );
        let stats = engine.verify();
        assert_eq!(
            stats.broken, 1,
            "verify обязан учитывать отсутствующую цель"
        );
        assert_eq!(engine.broken_keys(), vec!["absent.html".to_string()]);
        let _ = fs::remove_dir_all(&root);
    }

    /// П-4 / Д-140: HTML-страница под расширением ресурса (.ico/.css) считается непригодной в commit.
    #[test]
    fn commit_rejects_html_disguised_as_resource() {
        let root = workdir("htmlres");
        let item = Item {
            url: "https://example.com/favicon.ico".to_string(),
            ts: "20260101000000".to_string(),
            orig: "https://example.com/favicon.ico".to_string(),
            mime: "image/x-icon".to_string(),
            path: "favicon.ico".to_string(),
            kind: "img",
            len: 64,
            digest: String::new(),
            same_as: String::new(),
        };
        let tmp = root.join("mirror").join("favicon.ico.part");
        fs::create_dir_all(tmp.parent().unwrap()).unwrap();
        let body =
            b"<!DOCTYPE html><html><head><title>404</title></head><body>Not found</body></html>";
        fs::write(&tmp, body).unwrap();
        let engine = Engine::new("example.com", &root, 1, 0).unwrap();
        let saved = Saved {
            head: Head {
                status: 200,
                content_type: Some("text/html".to_string()),
                ..Default::default()
            },
            len: body.len() as u64,
            head_bytes: body.to_vec(),
            tmp: tmp.clone(),
        };
        assert!(engine.shared.commit(&item, &saved, 20).is_none());
        assert_eq!(engine.shared.counters.mismatched.load(Ordering::Relaxed), 1);
        assert!(!tmp.exists());
        let _ = fs::remove_dir_all(&root);
    }

    /// П-7 / Д-141: стратифицированный limit_order не забивает лимит вариантами одной рубрики.
    #[test]
    fn limit_order_diversifies_sections_and_prioritizes_assets() {
        let mk = |u: &str, p: &str, k: &'static str| Item {
            url: u.to_string(),
            ts: "20260101000000".to_string(),
            orig: u.to_string(),
            mime: String::new(),
            path: p.to_string(),
            kind: k,
            len: 100,
            digest: String::new(),
            same_as: String::new(),
        };
        let mut items = vec![mk("https://e.com/", "index.html", "html")];
        for i in 0..50 {
            items.push(mk(
                &format!("https://e.com/a/p?v={i}"),
                &format!("a/p__q_{i:04}.html"),
                "html",
            ));
        }
        items.push(mk("https://e.com/news/1.html", "news/1.html", "html"));
        items.push(mk("https://e.com/static/app.css", "static/app.css", "code"));

        let ordered = limit_order(items);
        let top10: Vec<&str> = ordered.iter().take(10).map(|i| i.path.as_str()).collect();
        assert_eq!(top10[0], "index.html");
        assert!(
            top10.contains(&"static/app.css"),
            "стили должны попадать в топ: {top10:?}"
        );
        assert!(
            top10.contains(&"news/1.html"),
            "другой раздел должен попадать в топ: {top10:?}"
        );
    }

    /// П-8 и П-9 / Д-142, Д-143: поддомены ассетов и минификатор `/min/?f=...` связываются локально.
    #[test]
    fn subdomains_and_minifier_links_resolve_locally() {
        let root = workdir("submin");
        put(
            &root,
            "index.html",
            b"<html><head><link rel=\"stylesheet\" href=\"//cdn.example.com/c/0.css\"><link rel=\"stylesheet\" href=\"/bld/min/?f=css/a.css,css/b.css\"></head></html>",
        );
        put(&root, "cdn.example.com/c/0.css", b"h1{color:red}");
        put(&root, "css/a.css", b".a{margin:0}");
        put(&root, "css/b.css", b".b{padding:0}");

        let engine = build(&root);
        let html = read(&root, "index.html");
        assert!(
            html.contains("href=\"cdn.example.com/c/0.css\""),
            "ссылка на поддомен должна переписаться: {html}"
        );
        assert!(
            html.contains("_min/"),
            "минификатор должен собраться в _min/: {html}"
        );
        assert_eq!(engine.minifier_built.load(Ordering::Relaxed), 1);
        let _ = fs::remove_dir_all(&root);
    }
}
