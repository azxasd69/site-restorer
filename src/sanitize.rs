//! Разбор того, что пользователь вставил в поле «Адрес сайта».
//!
//! Поддерживается: домен с схемой и без, путь/порт/точка на конце, текст вокруг
//! адреса, ссылка из веб-архива или поиска, адрес, набранный в русской раскладке,
//! кириллический домен (IDN -> punycode).

use regex_lite::Regex;
use std::sync::OnceLock;

/// ЙЦУКЕН -> QWERTY по физическим клавишам.
const RU_LAYOUT: &[(char, char)] = &[
    ('й', 'q'),
    ('ц', 'w'),
    ('у', 'e'),
    ('к', 'r'),
    ('е', 't'),
    ('н', 'y'),
    ('г', 'u'),
    ('ш', 'i'),
    ('щ', 'o'),
    ('з', 'p'),
    ('х', '['),
    ('ъ', ']'),
    ('ф', 'a'),
    ('ы', 's'),
    ('в', 'd'),
    ('а', 'f'),
    ('п', 'g'),
    ('р', 'h'),
    ('о', 'j'),
    ('л', 'k'),
    ('д', 'l'),
    ('ж', ';'),
    ('э', '\''),
    ('я', 'z'),
    ('ч', 'x'),
    ('с', 'c'),
    ('м', 'v'),
    ('и', 'b'),
    ('т', 'n'),
    ('ь', 'm'),
    ('б', ','),
    ('ю', '.'),
];

/// Откуда взялся адрес — для журнала.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Note {
    /// Обычный домен.
    Plain,
    /// Был набран в русской раскладке.
    Layout,
    /// Кириллический домен, переведён в punycode.
    Idn,
}

fn host_rx() -> &'static Regex {
    static RX: OnceLock<Regex> = OnceLock::new();
    RX.get_or_init(|| {
        Regex::new(
            r"^([a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)(\.[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)+$",
        )
        .expect("host regex")
    })
}

fn archive_rx() -> &'static Regex {
    static RX: OnceLock<Regex> = OnceLock::new();
    RX.get_or_init(|| Regex::new(r"/web/[^/]+/(https?://.+)$").expect("archive regex"))
}

fn archive_bare_rx() -> &'static Regex {
    static RX: OnceLock<Regex> = OnceLock::new();
    RX.get_or_init(|| Regex::new(r"/web/[^/]+/([^/].*)$").expect("archive bare regex"))
}

fn query_rx() -> &'static Regex {
    static RX: OnceLock<Regex> = OnceLock::new();
    RX.get_or_init(|| Regex::new(r"[?&](?:q|url|u)=([^&]+)").expect("query regex"))
}

fn latin_tld_rx() -> &'static Regex {
    static RX: OnceLock<Regex> = OnceLock::new();
    RX.get_or_init(|| Regex::new(r"^\.[a-z]{2,24}$").expect("tld regex"))
}

pub fn is_cyrillic(s: &str) -> bool {
    s.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c))
}

/// Текст, набранный в русской раскладке -> латинские клавиши.
pub fn dekeyboard(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let low = ch.to_lowercase().next().unwrap_or(ch);
        match RU_LAYOUT.iter().find(|(k, _)| *k == low) {
            Some((_, latin)) => {
                if ch.is_uppercase() {
                    out.extend(latin.to_uppercase());
                } else {
                    out.push(*latin);
                }
            }
            None => out.push(ch),
        }
    }
    out
}

fn hexval(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub fn pct_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hexval(bytes[i + 1]), hexval(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Убрать невидимый мусор и типографику от копипаста.
pub fn strip_invisible(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{feff}' | '\u{00ad}' => {}
            '\u{00a0}' | '\u{202f}' => out.push(' '),
            '«' | '»' | '“' | '”' | '„' => out.push('"'),
            '‘' | '’' => out.push('\''),
            '–' | '—' => out.push('-'),
            _ => out.push(ch),
        }
    }
    out.trim().to_string()
}

/// Достать настоящий адрес из ссылки архива или поисковика.
pub fn from_redirect(s: &str) -> String {
    let lower = s.to_lowercase();
    if lower.contains("web.archive.org/web/") {
        if let Some(m) = archive_rx().captures(s) {
            return m[1].to_string();
        }
        if let Some(m) = archive_bare_rx().captures(s) {
            return format!("http://{}", &m[1]);
        }
    }
    if ["google.", "yandex.", "bing.com", "duckduckgo.com"]
        .iter()
        .any(|h| lower.contains(h))
    {
        if let Some((_, query)) = s.split_once('?') {
            for pair in query.split('&') {
                if let Some((k, v)) = pair.split_once('=') {
                    if matches!(k, "q" | "url" | "u" | "target" | "text") && !v.is_empty() {
                        let cand = pct_decode(v);
                        if cand.contains("://") || cand.contains('.') {
                            return cand;
                        }
                    }
                }
            }
        }
        if let Some(m) = query_rx().captures(s) {
            return pct_decode(&m[1]);
        }
    }
    s.to_string()
}

/// Имя хоста из строки вида `https://user@host:8080/path?q=1`.
fn hostname_from_url(s: &str) -> Option<String> {
    let trimmed = s.trim();
    let lower = trimmed.to_ascii_lowercase();
    let mut rest = trimmed;
    // схема может быть набрана в любом регистре: HTTPS://Example.COM
    for scheme in ["https://", "http://", "//"] {
        if lower.starts_with(scheme) {
            rest = &trimmed[scheme.len()..];
            break;
        }
    }
    let end = rest
        .find(|c| c == '/' || c == '?' || c == '#')
        .unwrap_or(rest.len());
    let mut authority = &rest[..end];
    if let Some((_, host)) = authority.rsplit_once('@') {
        authority = host;
    }
    let host = match authority.rsplit_once(':') {
        Some((h, port)) if port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() => h,
        _ => authority,
    };
    let host = host.trim().trim_end_matches('.').to_lowercase();
    if host.is_empty() { None } else { Some(host) }
}

fn candidate_tokens(s: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let cleaned = s.replace(['\r', '\n'], " ");
    for raw in cleaned
        .split([' ', ',', ';', '\t'])
        .filter(|t| !t.is_empty())
    {
        let tok = raw.trim_matches(|c: char| "\"'`<>()[]{}.,;:!?".contains(c));
        if tok.is_empty() {
            continue;
        }
        let low = tok.to_lowercase();
        if low.starts_with("http://") || low.starts_with("https://") || tok.starts_with("//") {
            if let Some(host) = hostname_from_url(tok) {
                out.push((host, false));
                continue;
            }
        }
        if tok.contains('.') && !tok.contains(' ') {
            out.push((tok.split('/').next().unwrap_or(tok).to_string(), false));
        } else if is_cyrillic(tok) && dekeyboard(tok).contains('.') {
            out.push((dekeyboard(tok), true));
        }
    }
    out
}

fn valid_host(h: &str) -> bool {
    !h.is_empty() && h.len() <= 253 && host_rx().is_match(h)
}

fn finish(host: &str) -> Option<(String, Note)> {
    let mut host = host.trim().trim_matches('.').to_lowercase();
    if let Some(rest) = host.strip_prefix("www.") {
        host = rest.to_string();
    }
    if host.is_empty() {
        return None;
    }
    if !is_cyrillic(&host) {
        return if valid_host(&host) {
            Some((host.clone(), Note::Plain))
        } else {
            None
        };
    }

    // Кириллица: либо набрано в русской раскладке, либо настоящий IDN-домен.
    let dekeyed = dekeyboard(&host).to_lowercase().replace(',', ".");
    let tld = match host.rsplit_once('.') {
        Some((_, t)) => format!(".{t}"),
        None => String::new(),
    };
    let latin_tld = latin_tld_rx().is_match(&tld);
    let cyr_tld = tld.len() > 1 && is_cyrillic(&tld);

    if cyr_tld {
        if let Some(puny) = crate::puny::domain_to_ascii(&host) {
            if valid_host(&puny) {
                return Some((puny, Note::Idn));
            }
        }
    }
    if (latin_tld || valid_host(&dekeyed)) && valid_host(&dekeyed) {
        return Some((dekeyed, Note::Layout));
    }
    if let Some(puny) = crate::puny::domain_to_ascii(&host) {
        if valid_host(&puny) {
            return Some((puny, Note::Idn));
        }
    }
    if valid_host(&dekeyed) {
        return Some((dekeyed, Note::Layout));
    }
    None
}

/// Главная точка входа: адрес любого вида -> (домен, откуда он взялся).
pub fn sanitize_site(text: &str) -> Option<(String, Note)> {
    if text.trim().is_empty() {
        return None;
    }
    let s = from_redirect(&strip_invisible(text));

    // Сначала честный URL: схема + отсутствие пробелов.
    let looks_like_url = {
        let low = s.to_lowercase();
        (low.starts_with("http://") || low.starts_with("https://") || s.starts_with("//"))
            && !s.chars().any(|c| c.is_whitespace())
    };
    if looks_like_url {
        if let Some(host) = hostname_from_url(&s) {
            if host.contains('.') {
                if let Some(done) = finish(&host) {
                    return Some(done);
                }
            }
        }
    }

    let mut tokens = candidate_tokens(&s);
    if tokens.is_empty() {
        let dekeyed = dekeyboard(&s);
        if dekeyed.contains('.') {
            tokens.push((
                dekeyed
                    .split_whitespace()
                    .next()
                    .unwrap_or(&dekeyed)
                    .to_string(),
                true,
            ));
        }
    }
    let (first, dekeyed) = tokens.first()?;
    let mut done = finish(first)?;
    if *dekeyed && done.1 == Note::Plain {
        done.1 = Note::Layout;
    }
    Some(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(input: &str) -> Option<String> {
        sanitize_site(input).map(|(h, _)| h)
    }

    #[test]
    fn uppercase_scheme_and_host() {
        assert_eq!(host("HTTPS://RDBZH.RU/").as_deref(), Some("rdbzh.ru"));
        assert_eq!(
            host("HTTP://WWW.Example.COM/Path").as_deref(),
            Some("example.com")
        );
        assert_eq!(host("HtTpS://rdbzh.ru").as_deref(), Some("rdbzh.ru"));
    }

    #[test]
    fn common_forms() {
        let cases = [
            ("prokupol.ru", "prokupol.ru"),
            (
                "https://www.prokupol.ru/catalog/?utm_source=x#top",
                "prokupol.ru",
            ),
            ("  «prokupol.ru»  ", "prokupol.ru"),
            ("Сайт: prokupol.ru — магазин", "prokupol.ru"),
            (
                "https://web.archive.org/web/20261016223707/https://prokupol.ru/",
                "prokupol.ru",
            ),
            (
                "https://www.google.com/url?q=https%3A%2F%2Fprokupol.ru%2F&sa=D",
                "prokupol.ru",
            ),
            ("зкщлгзщдюкг", "prokupol.ru"),
            ("PROKUPOL.RU", "prokupol.ru"),
            ("http://prokupol.ru:8080/x", "prokupol.ru"),
            ("prokupol.ru.", "prokupol.ru"),
            ("https://prokupol.ru\nhttps://other.ru", "prokupol.ru"),
        ];
        for (raw, want) in cases {
            assert_eq!(host(raw).as_deref(), Some(want), "вход: {raw}");
        }
    }

    #[test]
    fn idn_becomes_punycode() {
        let (h, note) = sanitize_site("прокупол.рф").expect("idn");
        assert!(h.starts_with("xn--"), "{h}");
        assert_eq!(note, Note::Idn);
    }

    #[test]
    fn garbage_is_rejected() {
        for raw in ["", "   ", "prokupol", "http://"] {
            assert!(host(raw).is_none(), "вход: {raw}");
        }
    }

    #[test]
    fn layout_is_detected() {
        let (h, note) = sanitize_site("зкщлгзщдюкг").expect("layout");
        assert_eq!(h, "prokupol.ru");
        assert_eq!(note, Note::Layout);
    }
}
