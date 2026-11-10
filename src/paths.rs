//! Адрес -> путь на диске. Детерминированно, с оглядкой на ограничения файловых систем.

use crate::sanitize::pct_decode;

/// Предел длины относительного пути: длиннее — сворачиваем в короткое имя.
pub const MAX_REL_LEN: usize = 160;
/// Предел длины одного сегмента до хэширования.
const MAX_SEG_LEN: usize = 80;
const RESERVED: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Разобранный адрес (минимум, который нужен планировщику путей).
#[derive(Debug, Clone, Default)]
pub struct Parts {
    pub host: String,
    pub port: Option<String>,
    pub path: String,
    pub query: String,
}

pub fn parse(url: &str) -> Parts {
    let mut s = url.split('#').next().unwrap_or("").trim().to_string();
    if s.starts_with("//") {
        s = format!("https:{s}");
    }
    let after_scheme = match s.find("://") {
        Some(i) => s[i + 3..].to_string(),
        None => s.clone(),
    };
    let (authority, rest) = match after_scheme.find(['/', '?']) {
        Some(i) => (after_scheme[..i].to_string(), after_scheme[i..].to_string()),
        None => (after_scheme.clone(), String::new()),
    };
    let (path, query) = match rest.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (rest, String::new()),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
            (h.to_string(), Some(p.to_string()))
        }
        _ => (authority, None),
    };
    Parts {
        host: host.to_lowercase(),
        port,
        path,
        query,
    }
}

/// Стабильный хэш (FNV-1a 32) — не зависит от версии Rust и платформы.
pub fn fnv_hex(s: &str) -> String {
    let mut hash: u32 = 0x811c_9dc5;
    for b in s.as_bytes() {
        hash ^= *b as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("{hash:08x}")
}

/// Один сегмент пути: без запрещённых символов, не длиннее лимита.
pub fn safe_seg(seg: &str) -> String {
    let decoded = pct_decode(seg);
    let mut cleaned = String::with_capacity(decoded.len());
    for ch in decoded.chars() {
        let bad = matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '\\' | '/')
            || (ch as u32) < 0x20;
        cleaned.push(if bad { '_' } else { ch });
    }
    let mut seg = cleaned
        .trim_matches(|c: char| c == ' ' || c == '.')
        .to_string();
    if seg.chars().count() > MAX_SEG_LEN {
        let cut: String = seg.chars().take(60).collect();
        seg = format!("{cut}_{}", fnv_hex(&seg));
    }
    if seg.is_empty() {
        seg = "_".to_string();
    }
    let base = seg.split('.').next().unwrap_or("").to_lowercase();
    if RESERVED.contains(&base.as_str()) {
        seg = format!("_{seg}");
    }
    seg
}

/// Адрес -> относительный путь файла в копии. Один адрес всегда даёт один путь.
pub fn url_to_local_path(url: &str) -> String {
    let parts = parse(url);
    let mut segs: Vec<String> = parts
        .path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(safe_seg)
        .collect();

    if !parts.query.is_empty() {
        let hash = fnv_hex(&parts.query);
        if segs.is_empty() {
            segs.push("index".to_string());
        }
        let last = segs.last().cloned().unwrap_or_default();
        let rewritten = match last.rfind('.') {
            Some(i) if last.len() - i <= 6 => format!("{}__q_{}{}", &last[..i], hash, &last[i..]),
            _ => format!("{last}__q_{hash}"),
        };
        if let Some(slot) = segs.last_mut() {
            *slot = rewritten;
        }
    }

    if segs.is_empty() {
        return "index.html".to_string();
    }
    let last = segs.last().cloned().unwrap_or_default();
    if parts.path.ends_with('/') || !last.contains('.') {
        segs.push("index.html".to_string());
    }

    let rel = segs.join("/");
    if rel.chars().count() > MAX_REL_LEN {
        // Патологические адреса (data-URI, очень глубокие деревья): короткое детерминированное имя.
        let head = if segs.len() > 1 {
            segs[0].clone()
        } else {
            "_".to_string()
        };
        let ext: String = match segs.last().and_then(|s| s.rfind('.').map(|i| &s[i..])) {
            Some(e) => e.chars().take(6).collect(),
            None => String::new(),
        };
        return format!("{head}/_long/{}{ext}", fnv_hex(url));
    }
    rel
}

/// Привести ссылку к виду `https://host/path?query` без якоря и лишних портов.
pub fn normalize_url(url: &str, host: &str) -> String {
    let mut u = url.trim().split('#').next().unwrap_or("").to_string();
    if u.starts_with("//") {
        u = format!("https:{u}");
    }
    if !u.contains("://") {
        u = format!("https://{host}/{}", u.trim_start_matches('/'));
    }
    let parts = parse(&u);
    let mut netloc = if parts.host.is_empty() {
        host.to_string()
    } else {
        parts.host.clone()
    };
    if let Some(rest) = netloc.clone().strip_prefix("www.") {
        if rest == host {
            netloc = rest.to_string();
        }
    }
    if let Some(port) = &parts.port {
        if port != "80" && port != "443" {
            netloc = format!("{netloc}:{port}");
        }
    }
    let path = if parts.path.is_empty() {
        "/"
    } else {
        &parts.path
    };
    if parts.query.is_empty() {
        format!("https://{netloc}{path}")
    } else {
        format!("https://{netloc}{path}?{}", parts.query)
    }
}

/// Тип файла для полос прогресса и отчёта.
pub fn file_type(url: &str) -> &'static str {
    let parts = parse(url);
    let ext = parts
        .path
        .rsplit_once('.')
        .map(|(_, e)| e.to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "svg" | "ico" | "bmp" => "img",
        "css" | "js" | "json" | "xml" | "woff" | "woff2" | "ttf" => "code",
        "" | "html" | "htm" => "html",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_paths() {
        assert_eq!(
            url_to_local_path("https://example.com/catalog/tovar/"),
            "catalog/tovar/index.html"
        );
        assert_eq!(url_to_local_path("https://example.com/"), "index.html");
    }

    #[test]
    fn query_is_hashed_and_stable() {
        let a = url_to_local_path("https://example.com/list?page=2&sort=asc");
        let b = url_to_local_path("https://example.com/list?page=2&sort=asc");
        assert_eq!(a, b);
        assert!(a.contains("__q_"), "{a}");
        assert_ne!(a, url_to_local_path("https://example.com/list?page=3"));
    }

    #[test]
    fn long_paths_are_capped() {
        let deep = format!(
            "https://example.com/{}",
            (0..60)
                .map(|i| format!("segment{i:02}"))
                .collect::<Vec<_>>()
                .join("/")
        );
        assert!(url_to_local_path(&deep).chars().count() <= MAX_REL_LEN);
    }

    #[test]
    fn data_uri_is_capped() {
        let url = format!(
            "https://example.com/mirror/_data_image_gif;base64,{}",
            "A".repeat(4000)
        );
        assert!(url_to_local_path(&url).chars().count() <= MAX_REL_LEN);
    }

    #[test]
    fn reserved_names_are_escaped() {
        let rel = url_to_local_path("https://example.com/con/prn/aux/nul.txt");
        for seg in rel.split('/') {
            let base = seg.split('.').next().unwrap_or("").to_lowercase();
            assert!(!RESERVED.contains(&base.as_str()), "сегмент: {seg}");
        }
    }

    #[test]
    fn traversal_is_neutralised() {
        for url in [
            "https://example.com/../../etc/passwd",
            "https://example.com/%2e%2e/%2e%2e/win.ini",
        ] {
            let rel = url_to_local_path(url);
            assert!(!rel.contains(".."), "{url} -> {rel}");
        }
    }

    #[test]
    fn normalize_drops_www_and_anchor() {
        assert_eq!(
            normalize_url("https://www.example.com/a/b#top", "example.com"),
            "https://example.com/a/b"
        );
        assert_eq!(
            normalize_url("/catalog/", "example.com"),
            "https://example.com/catalog/"
        );
        assert_eq!(
            normalize_url("http://example.com:8080/x", "example.com"),
            "https://example.com:8080/x"
        );
    }

    #[test]
    fn types_by_extension() {
        assert_eq!(file_type("https://e.com/a.png"), "img");
        assert_eq!(file_type("https://e.com/a.css"), "code");
        assert_eq!(file_type("https://e.com/page"), "html");
        assert_eq!(file_type("https://e.com/f.pdf"), "other");
    }
}
