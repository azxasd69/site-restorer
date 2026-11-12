//! Кодировки: выбор, декодирование и нормализация в UTF-8.
//!
//! Порядок принятия решения (первое достоверное — побеждает):
//! 1. BOM (UTF-8 / UTF-16LE / UTF-16BE) — байты сильнее любых объявлений.
//! 2. Валидный UTF-8 — почти всегда он и есть; отдельно ловим «двойное
//!    кодирование» (UTF-8, прочитанный как windows-1251/KOI8-R).
//! 3. Объявления: `Content-Type` из ответа, `<meta charset>`, `<?xml encoding?>`,
//!    `@charset` в CSS.
//! 4. Определение по содержимому (chardetng: статистика Mozilla).
//!
//! Кандидаты (объявленный и определённый) декодируются и сравниваются оценкой
//! качества: доля мусорных символов + близость частот букв к русскому языку.
//! Объявлению даётся небольшой бонус — но не абсолютный приоритет: заголовки
//! веб-архива регулярно не соответствуют телу (например, KOI8-R под видом
//! windows-1251), а объявление может стоять в комментарии.

use encoding_rs::{Encoding, UTF_16BE, UTF_16LE, WINDOWS_1251};
use regex::Regex;
use std::sync::OnceLock;

/// Результат декодирования документа.
#[derive(Debug, Clone)]
pub struct Decoded {
    pub text: String,
    /// Кодировка, которая применена на самом деле.
    pub label: &'static str,
    /// Откуда взята: bom / utf8 / mojibake / header / meta / xml / css / detected / fallback.
    pub source: &'static str,
    /// 0..100, грубая уверенность (для журнала и отчёта).
    pub confidence: u8,
}

impl Decoded {
    fn new(text: String, label: &'static str, source: &'static str, confidence: u8) -> Self {
        Self {
            text,
            label,
            source,
            confidence,
        }
    }

    pub fn was_converted(&self) -> bool {
        !matches!(self.source, "utf8" | "bom-utf8")
    }
}

/// Декодировать документ в UTF-8. `content_type` — заголовок ответа, если он есть.
pub fn decode_document(data: &[u8], content_type: Option<&str>) -> Decoded {
    // 1. BOM
    if let Some(rest) = data.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return Decoded::new(
            String::from_utf8_lossy(rest).into_owned(),
            "utf-8",
            "bom-utf8",
            100,
        );
    }
    if let Some(rest) = data.strip_prefix(&[0xFF, 0xFE]) {
        let (text, _, _) = UTF_16LE.decode(rest);
        return Decoded::new(text.into_owned(), "utf-16le", "bom", 100);
    }
    if let Some(rest) = data.strip_prefix(&[0xFE, 0xFF]) {
        let (text, _, _) = UTF_16BE.decode(rest);
        return Decoded::new(text.into_owned(), "utf-16be", "bom", 100);
    }

    // 2. Валидный UTF-8
    if let Ok(text) = std::str::from_utf8(data) {
        if let Some(fixed) = repair_mojibake(text) {
            return Decoded::new(fixed, "utf-8", "mojibake", 85);
        }
        return Decoded::new(text.to_string(), "utf-8", "utf8", 100);
    }

    // 3. Объявленные кодировки
    let declared = declared_encoding(data, content_type);
    let detected = detect_encoding(data);

    let mut best: Option<(Decoded, f64)> = None;
    for (enc, source, bonus, declared_here) in [
        declared.map(|(e, s)| (e, s, 0.04, true)),
        detected.map(|e| (e, "detected", 0.0, false)),
    ]
    .into_iter()
    .flatten()
    {
        let (text, _, had_errors) = enc.decode(data);
        // Объявленная иероглифическая кодировка с чистой разборкой: русская
        // частотная проверка ничего не измеряет на таком тексте (кириллицы нет,
        // иероглифы считаются мусором), а запасной windows-1251 портит страницу
        // целиком. Принимаем объявление, если иероглифы действительно есть.
        if declared_here && !had_errors && is_cjk_family(enc) && has_cjk(&text) {
            return Decoded::new(text.into_owned(), enc_label(enc), source, 90);
        }
        let mut score = quality(&text);
        if had_errors {
            score -= 0.15;
        }
        score += bonus;
        let cand = Decoded::new(
            text.into_owned(),
            enc_label(enc),
            source,
            (score.clamp(0.0, 1.0) * 100.0) as u8,
        );
        if best.as_ref().is_none_or(|(_, s)| score > *s) {
            best = Some((cand, score));
        }
    }

    match best {
        Some((cand, score)) if score > 0.35 => cand,
        // совсем плохой результат — лучше windows-1251, как в 90-е
        _ => {
            let (text, _, _) = WINDOWS_1251.decode(data);
            Decoded::new(text.into_owned(), "windows-1251", "fallback", 20)
        }
    }
}

/// Стабильное имя кодировки для журнала.
fn enc_label(enc: &'static Encoding) -> &'static str {
    match enc.name() {
        "windows-1251" => "windows-1251",
        "KOI8-R" => "koi8-r",
        "KOI8-U" => "koi8-u",
        "IBM866" => "ibm866",
        "ISO-8859-5" => "iso-8859-5",
        "windows-1252" => "windows-1252",
        "UTF-8" => "utf-8",
        "UTF-16LE" => "utf-16le",
        "UTF-16BE" => "utf-16be",
        other => leak(other),
    }
}

fn leak(name: &str) -> &'static str {
    // Имён конечное число; утечка на процесс безопасна.
    Box::leak(name.to_string().into_boxed_str())
}

/// Нормализация метки: `cp1251`, `win-1251`, `koi8r`, `utf8` и т.п.
pub fn encoding_for_label(raw: &str) -> Option<&'static Encoding> {
    let label = raw
        .trim()
        .trim_matches(['"', '\'', ';'])
        .to_ascii_lowercase();
    let label = label.replace(' ', "");
    let aliased = match label.as_str() {
        "cp1251" | "cp-1251" | "win1251" | "win-1251" | "windows1251" | "x-cp1251" | "ansi"
        | "windows-cyrillic" => "windows-1251",
        "utf8" | "utf-8" | "unicode-1-1-utf-8" => "utf-8",
        "koi8r" | "koi8-r" => "koi8-r",
        "koi8u" | "koi8-u" => "koi8-u",
        "cp866" | "ibm866" | "866" => "ibm866",
        "iso8859-5" | "iso-8859-5" | "iso88595" => "iso-8859-5",
        "cp1252" | "windows-1252" | "latin1" | "iso-8859-1" => "windows-1252",
        other => other,
    };
    Encoding::for_label(aliased.as_bytes())
}

/// Объявленная кодировка: заголовок ответа, meta, xml, css — в порядке надёжности.
fn declared_encoding(
    data: &[u8],
    content_type: Option<&str>,
) -> Option<(&'static Encoding, &'static str)> {
    if let Some(ct) = content_type
        && let Some(enc) = charset_from_header(ct)
    {
        return Some((enc, "header"));
    }
    let sniff = sniff_local(data);
    if let Some((enc, source)) = sniff {
        return Some((enc, source));
    }
    None
}

/// charset из `Content-Type`.
pub fn charset_from_header(value: &str) -> Option<&'static Encoding> {
    let lower = value.to_ascii_lowercase();
    let idx = lower.find("charset=")?;
    let tail = &lower[idx + "charset=".len()..];
    let label: String = tail
        .chars()
        .skip_while(|c| c.is_whitespace() || *c == '"' || *c == '\'')
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
        .collect();
    if label.is_empty() {
        return None;
    }
    encoding_for_label(&label)
}

/// Объявления в самом документе.
fn sniff_local(data: &[u8]) -> Option<(&'static Encoding, &'static str)> {
    // Смотрим первые 64 КБ: встречаются страницы, где meta стоит далеко от начала.
    let window = &data[..data.len().min(65_536)];
    let text = String::from_utf8_lossy(window);
    let cleaned = mask_comments_and_scripts(&text);

    // <?xml version="1.0" encoding="windows-1251"?>
    if let Some(caps) = xml_decl_rx().captures(&cleaned)
        && let Some(enc) = encoding_for_label(&caps[1])
    {
        return Some((enc, "xml"));
    }
    if let Some(caps) = meta_charset_rx().captures(&cleaned)
        && let Some(enc) = encoding_for_label(&caps[1])
    {
        return Some((enc, "meta"));
    }
    if let Some(caps) = css_charset_rx().captures(&cleaned)
        && let Some(enc) = encoding_for_label(&caps[1])
    {
        return Some((enc, "css"));
    }
    None
}

fn mask_comments_and_scripts(text: &str) -> String {
    let mut out = text.to_string();
    for rx in [comment_rx(), script_rx()] {
        out = rx.replace_all(&out, " ").into_owned();
    }
    out
}

fn xml_decl_rx() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#"(?is)<\?xml[^>]*encoding\s*=\s*["']([^"']+)["']"#).unwrap())
}
fn meta_charset_rx() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#"(?is)<meta[^>]+charset\s*=\s*["']?\s*([a-z0-9_.:-]+)"#).unwrap())
}
fn css_charset_rx() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#"(?is)@charset\s+["']([^"']+)["']"#).unwrap())
}
fn comment_rx() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?s)<!--.*?-->").unwrap())
}
fn script_rx() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"(?is)<script\b[^>]*>.*?</script\s*>|<style\b[^>]*>.*?</style\s*>").unwrap()
    })
}

/// Определение кодировки по содержимому: перебираем кириллические варианты и
/// берём тот, что даёт самый «читаемый» текст. Практически это то же, что делал
/// chardetng (статистика по байтам), но без отдельной зависимости и без ICU.
fn detect_encoding(data: &[u8]) -> Option<&'static Encoding> {
    if data.len() < 8 {
        return None;
    }
    const CANDIDATES: [&Encoding; 6] = [
        WINDOWS_1251,
        encoding_rs::KOI8_R,
        encoding_rs::KOI8_U,
        encoding_rs::IBM866,
        encoding_rs::ISO_8859_5,
        encoding_rs::WINDOWS_1252,
    ];
    let mut best: Option<(&'static Encoding, f64)> = None;
    for enc in CANDIDATES {
        let (text, _, _) = enc.decode(data);
        let score = quality(&text);
        if best.is_none_or(|(_, s)| score > s) {
            best = Some((enc, score));
        }
    }
    best.map(|(enc, _)| enc)
}

/// «Двойное кодирование»: UTF-8, прочитанный как однобайтовая кириллица,
/// встречается почти на каждом шагу в старых базах. Признаки — характерные
/// пары вида `ÐŸ`, `Ñ€`, `â€`, `ïðè`.
fn repair_mojibake(text: &str) -> Option<String> {
    if text.len() > 400_000 {
        return None;
    }
    if !looks_like_mojibake(text) {
        return None;
    }
    let mut best: Option<(String, f64)> = None;
    for enc in [WINDOWS_1251, encoding_rs::KOI8_R, encoding_rs::WINDOWS_1252] {
        let (bytes, _, _) = enc.encode(text);
        if let Ok(restored) = std::str::from_utf8(&bytes) {
            let score = quality(restored);
            if score > 0.55 && best.as_ref().is_none_or(|(_, s)| score > *s) {
                best = Some((restored.to_string(), score));
            }
        }
    }
    best.map(|(text, _)| text)
}

/// Признак «двойного кодирования»: UTF-8, прочитанный как однобайтовая
/// кодировка. Русская буква в UTF-8 — два байта, поэтому в таком тексте
/// аномально много заглавных «Р»/«С» (0xD0/0xD1 в windows-1251) или «Ð»/«Ñ»
/// (latin1), а также характерные обрывки вроде «вЂ».
fn looks_like_mojibake(text: &str) -> bool {
    let sample: String = text.chars().take(4000).collect();
    let total = sample.chars().count() as f64;
    if total < 20.0 {
        return false;
    }
    let rs = sample
        .chars()
        .filter(|c| matches!(c, 'Р' | 'С' | 'Ð' | 'Ñ' | 'Œ' | '€' | '‚'))
        .count() as f64;
    let artefacts = sample.matches("вЂ").count() + sample.matches("ÐŸ").count();
    rs / total > 0.22 || artefacts >= 2
}

/// Похоже ли содержимое на двоичное. Нужно, потому что в архиве картинка или
/// PDF нередко лежит под адресом с расширением `.html`: если прогнать такие
/// байты через текстовое декодирование, файл будет испорчен.
pub fn looks_binary(data: &[u8]) -> bool {
    const MAGIC: [&[u8]; 12] = [
        &[0xFF, 0xD8, 0xFF],       // JPEG
        &[0x89, 0x50, 0x4E, 0x47], // PNG
        &[0x47, 0x49, 0x46, 0x38], // GIF
        &[0x25, 0x50, 0x44, 0x46], // PDF
        &[0x50, 0x4B, 0x03, 0x04], // ZIP/ODT/DOCX
        &[0x77, 0x4F, 0x46, 0x46], // WOFF
        &[0x77, 0x4F, 0x46, 0x32], // WOFF2
        &[0x52, 0x49, 0x46, 0x46], // RIFF (webp/wav)
        &[0x1F, 0x8B],             // gzip
        &[0x42, 0x4D],             // BMP
        &[0x4D, 0x5A],             // PE
        &[0x7F, 0x45, 0x4C, 0x46], // ELF
    ];
    if MAGIC.iter().any(|m| data.starts_with(m)) {
        return true;
    }
    let head = &data[..data.len().min(4096)];
    if head.is_empty() {
        return false;
    }
    if head.contains(&0u8) {
        return true;
    }
    let control = head
        .iter()
        .filter(|b| **b < 0x09 || (**b > 0x0D && **b < 0x20))
        .count();
    control * 100 / head.len() > 8
}

/// Оценка «похожести на нормальный текст» 0..1.
pub fn quality(text: &str) -> f64 {
    let sample: String = text.chars().take(20_000).collect();
    if sample.is_empty() {
        return 0.0;
    }
    let mut good = 0.0f64;
    let mut bad = 0.0f64;
    let mut total = 0.0f64;
    for ch in sample.chars() {
        total += 1.0;
        match ch {
            'а'..='я' | 'А'..='Я' | 'ё' | 'Ё' => good += 1.0,
            ' ' | '\n' | '\r' | '\t' => good += 0.35,
            '—' | '–' | '«' | '»' | '…' | '№' | '’' | '“' | '”' | '°' | '±' | '×' => {
                good += 0.7
            }
            c if c.is_ascii_alphanumeric() => good += 0.45,
            c if c.is_ascii_punctuation() => good += 0.35,
            '\u{0080}'..='\u{009F}' | '\u{FFFD}' => bad += 3.0,
            // латиница с диакритикой в русском тексте — почти всегда следствие
            // неверной кодировки (windows-1252 вместо кириллицы)
            '\u{00A0}'..='\u{00FF}' => bad += 1.2,
            c if (c as u32) > 0x2FFF && !is_cjk_punct(c) => bad += 1.5,
            _ => bad += 0.25,
        }
    }
    let base = ((good - bad) / total).clamp(0.0, 1.0);
    // близость частот букв к русскому языку: перепутанная кодировка даёт
    // заметно другую гистограмму
    let freq = chi_square_ru(&sample);
    // масштаб 2.5 подобран на реальных страницах: верная кодировка даёт
    // chi ~0.5, перепутанная — 2.4 и выше
    let freq_score = 1.0 / (1.0 + freq / 2.5);
    (base * 0.5 + freq_score * 0.5).clamp(0.0, 1.0)
}

fn is_cjk_punct(c: char) -> bool {
    matches!(c, '\u{3000}'..='\u{303F}')
}

/// Иероглифические семейства: кириллическая частотная проверка к ним неприменима.
fn is_cjk_family(enc: &'static Encoding) -> bool {
    matches!(
        enc.name(),
        "Shift_JIS" | "EUC-JP" | "ISO-2022-JP" | "GBK" | "gb18030" | "Big5" | "EUC-KR"
    )
}

/// Есть ли в тексте иероглифы: признак, что байты разобраны той кодировкой,
/// которую объявила страница, а не случайной.
fn has_cjk(text: &str) -> bool {
    text.chars().any(|c| {
        matches!(
            c as u32,
            0x3040..=0x30FF
                | 0x3400..=0x4DBF
                | 0x4E00..=0x9FFF
                | 0xAC00..=0xD7AF
                | 0xF900..=0xFAFF
        )
    })
}

/// Хи-квадрат гистограммы русских букв относительно эталонных частот (на 1000).
pub fn chi_square_ru(text: &str) -> f64 {
    const FREQ: [(char, f64); 33] = [
        ('о', 109.7),
        ('е', 84.5),
        ('а', 80.1),
        ('и', 73.5),
        ('н', 67.0),
        ('т', 62.6),
        ('с', 54.7),
        ('р', 47.3),
        ('в', 45.4),
        ('л', 44.0),
        ('к', 34.9),
        ('м', 32.1),
        ('д', 29.8),
        ('п', 28.1),
        ('у', 26.2),
        ('я', 20.1),
        ('ы', 19.0),
        ('ь', 17.4),
        ('г', 17.0),
        ('з', 16.5),
        ('б', 15.9),
        ('ч', 14.4),
        ('й', 12.1),
        ('х', 9.7),
        ('ж', 9.4),
        ('ш', 7.3),
        ('ю', 6.4),
        ('ц', 4.8),
        ('щ', 3.6),
        ('э', 3.2),
        ('ф', 2.6),
        ('ъ', 0.4),
        ('ё', 0.4),
    ];
    let mut counts = [0f64; 33];
    let mut letters = 0f64;
    for ch in text.chars() {
        let lower = ch.to_lowercase().next().unwrap_or(ch);
        for (i, (ch, _)) in FREQ.iter().enumerate() {
            if *ch == lower {
                counts[i] += 1.0;
                letters += 1.0;
                break;
            }
        }
    }
    if letters < 40.0 {
        return 6.0;
    }
    let mut chi = 0.0;
    for (i, (_, expected)) in FREQ.iter().enumerate() {
        // эталон дан на 1000 букв; приводим к масштабу документа
        let exp = expected / 1000.0 * letters;
        if exp <= 0.0 {
            continue;
        }
        let diff = counts[i] - exp;
        chi += diff * diff / exp;
    }
    chi / letters
}

/// Нормализация документа после декодирования: объявления → utf-8 и, если
/// charset не попал в первые 1024 байта (браузер дальше не смотрит),
/// собственный `<meta charset="utf-8">` сразу после `<head>`.
pub fn normalize_utf8(text: &str) -> String {
    let out = any_charset_rx().replace_all(text, "${1}utf-8").into_owned();
    let out = any_css_charset_rx()
        .replace_all(&out, "${1}utf-8${3}")
        .into_owned();
    ensure_charset_in_head(&out)
}

fn any_charset_rx() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#"(?i)(charset\s*=\s*["']?\s*)([a-z0-9_.:\-]+)"#).unwrap())
}
fn any_css_charset_rx() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#"(?i)(@charset\s+["'])([^"']+)(["'])"#).unwrap())
}
fn head_rx() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)<head\b[^>]*>").unwrap())
}

fn ensure_charset_in_head(text: &str) -> String {
    const LIMIT: usize = 1024;
    let declared = any_charset_rx()
        .find(text)
        .map(|m| m.start())
        .unwrap_or(usize::MAX);
    if declared < LIMIT {
        return text.to_string();
    }
    let Some(head_end) = head_rx().find(text).map(|m| m.end()) else {
        return text.to_string();
    };
    let mut out = String::with_capacity(text.len() + 32);
    out.push_str(&text[..head_end]);
    out.push_str("<meta charset=\"utf-8\">");
    out.push_str(&text[head_end..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Старая страница в shift_jis остаётся в shift_jis: русская частотная
    /// проверка её не понимает, а запасной windows-1251 делает из текста мусор.
    #[test]
    fn declared_japanese_page_is_not_replaced_by_cp1251() {
        // «こんにちは» в Shift_JIS.
        let body = [0x82u8, 0xB1, 0x82, 0xF1, 0x82, 0xC9, 0x82, 0xBF, 0x82, 0xCD];
        let decoded = decode_document(&body, Some("text/html; charset=shift_jis"));
        assert_eq!(
            decoded.label.to_lowercase(),
            "shift_jis",
            "объявленная кодировка сохранена"
        );
        assert_eq!(decoded.text, "こんにちは");

        // Без объявления кодировки правило не действует: остаётся прежний путь.
        let blind = decode_document(&body, None);
        assert_ne!(
            blind.label.to_lowercase(),
            "shift_jis",
            "на угадывание правило не распространяется"
        );
    }

    const RU: &str = "Съешь же ещё этих мягких французских булок, да выпей чаю. \
Правительство Российской Федерации объявило о новых мерах поддержки малого бизнеса \
в регионах страны. По словам министра, изменения вступят в силу с первого января.";

    fn encoded(enc: &'static Encoding) -> Vec<u8> {
        let (bytes, _, _) = enc.encode(RU);
        bytes.into_owned()
    }

    #[test]
    fn cp1251_without_any_declaration() {
        let d = decode_document(&encoded(WINDOWS_1251), None);
        assert_eq!(d.label, "windows-1251");
        assert!(
            d.text.starts_with("Съешь"),
            "получилось: {}",
            &d.text[..40.min(d.text.len())]
        );
    }

    #[test]
    fn koi8r_detected_even_though_header_lies() {
        // архив регулярно врёт: заголовок windows-1251, тело — KOI8-R
        let d = decode_document(
            &encoded(encoding_rs::KOI8_R),
            Some("text/html; charset=windows-1251"),
        );
        assert!(
            d.text.starts_with("Съешь"),
            "кодировка {}: {}",
            d.label,
            &d.text[..40.min(d.text.len())]
        );
        assert_ne!(d.label, "windows-1251");
    }

    #[test]
    fn cp866_and_iso8859_5_are_recognised() {
        for enc in [encoding_rs::IBM866, encoding_rs::ISO_8859_5] {
            let d = decode_document(&encoded(enc), None);
            assert!(
                d.text.starts_with("Съешь"),
                "{}: {}",
                enc.name(),
                &d.text[..30.min(d.text.len())]
            );
        }
    }

    #[test]
    fn utf16_with_bom() {
        let mut bytes = vec![0xFF, 0xFE];
        for unit in RU.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let d = decode_document(&bytes, None);
        assert_eq!(d.source, "bom");
        assert!(d.text.starts_with("Съешь"));
    }

    #[test]
    fn utf8_bom_is_stripped() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(RU.as_bytes());
        let d = decode_document(&bytes, None);
        assert_eq!(d.source, "bom-utf8");
        assert!(!d.text.starts_with('\u{FEFF}'));
    }

    #[test]
    fn header_charset_wins_over_missing_meta() {
        let d = decode_document(&encoded(WINDOWS_1251), Some("text/html; charset=cp1251"));
        assert_eq!(d.label, "windows-1251");
        assert_eq!(d.source, "header");
    }

    #[test]
    fn meta_inside_comment_is_ignored() {
        let mut page = encoded(WINDOWS_1251);
        let prefix = b"<html><head><!-- <meta charset=\"utf-8\"> --></head><body>".to_vec();
        let suffix = b"</body></html>".to_vec();
        page = [prefix, page, suffix].concat();
        let d = decode_document(&page, None);
        assert_eq!(
            d.label, "windows-1251",
            "метка из комментария не должна побеждать"
        );
        assert!(d.text.contains("Съешь"));
    }

    #[test]
    fn mojibake_is_repaired() {
        // UTF-8 → прочитали как windows-1251 → снова записали в UTF-8
        let (wrong, _, _) = WINDOWS_1251.decode(RU.as_bytes());
        let broken = wrong.into_owned();
        assert!(
            broken.contains('Р') || broken.contains('С'),
            "нет признаков mojibake: {}",
            &broken[..40.min(broken.len())]
        );
        let d = decode_document(broken.as_bytes(), None);
        assert_eq!(d.source, "mojibake");
        assert!(
            d.text.starts_with("Съешь"),
            "получилось: {}",
            &d.text[..40.min(d.text.len())]
        );
    }

    #[test]
    fn xml_declaration_is_honoured() {
        let mut page = b"<?xml version=\"1.0\" encoding=\"windows-1251\"?><html><body>".to_vec();
        page.extend_from_slice(&encoded(WINDOWS_1251));
        let d = decode_document(&page, None);
        assert_eq!(d.source, "xml");
        assert!(d.text.contains("Съешь"));
    }

    #[test]
    fn charset_labels_are_normalised() {
        for label in ["cp1251", "win-1251", "windows1251", "Windows-1251", "ansi"] {
            assert_eq!(
                encoding_for_label(label).map(|e| e.name()),
                Some("windows-1251"),
                "метка {label}"
            );
        }
        for label in ["utf8", "UTF-8", "unicode-1-1-utf-8"] {
            assert_eq!(encoding_for_label(label).map(|e| e.name()), Some("UTF-8"));
        }
        assert_eq!(
            encoding_for_label("koi8r").map(|e| e.name()),
            Some("KOI8-R")
        );
    }

    #[test]
    fn deep_meta_gets_inserted_into_head() {
        let mut page = String::from("<html><head><script>var a=1;</script>");
        page.push_str(&"<!-- filler -->".repeat(90));
        page.push_str("<meta charset=\"windows-1251\"></head><body>привет</body></html>");
        let out = normalize_utf8(&page);
        let at = out
            .find("<meta charset=\"utf-8\">")
            .expect("meta вставлена");
        assert!(at < 1024);
        assert!(!out.contains("windows-1251"));
    }

    #[test]
    fn binary_detection() {
        assert!(looks_binary(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10]));
        assert!(looks_binary(b"%PDF-1.7\n"));
        assert!(looks_binary(b"text\0with\0nul"));
        assert!(!looks_binary(
            b"<html><body>\xd0\x9f\xd1\x80\xd0\xb8\xd0\xb2\xd0\xb5\xd1\x82</body></html>"
        ));
        assert!(!looks_binary(RU.as_bytes()));
    }

    #[test]
    fn quality_prefers_correct_decoding() {
        let good = quality(RU);
        let bad = quality(&WINDOWS_1251.decode(&encoded(encoding_rs::KOI8_R)).0);
        assert!(
            good > bad + 0.10,
            "good={good:.3} bad={bad:.3} chi_good={:.2} chi_bad={:.2}",
            chi_square_ru(RU),
            chi_square_ru(&WINDOWS_1251.decode(&encoded(encoding_rs::KOI8_R)).0)
        );
    }
}
