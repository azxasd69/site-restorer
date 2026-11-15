//! Встроенный локальный HTTP-сервер для просмотра собранной копии (Д-49, ТЗ 4.2.10).
//!
//! Без сторонних зависимостей: чистый `std::net::TcpListener` с пулом на
//! 4 воркера. Слушает только `127.0.0.1` (БЗ-13), принимает только `GET` и `HEAD`,
//! отображает адреса в файлы теми же правилами, что и сборка (`paths::url_to_local_path`),
//! поддерживает `Range` (`206`/`416`) и `ETag` (`304`).

use crate::core::long_path;
use crate::paths;
use crate::sanitize::pct_decode;
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, UNIX_EPOCH};

const WORKER_COUNT: usize = 4;
const MAX_HEADER_BYTES: usize = 16 * 1024;
const COPY_BUF_SIZE: usize = 64 * 1024;

/// Определить каталог готовой копии: пользователь может указать как корень
/// результата (где лежат `site/`, `mirror/`, `data/`, `report.html`), так и
/// непосредственно папку `site/`.
pub fn resolve_site_dir(path: &Path) -> Option<PathBuf> {
    let trimmed = path.as_os_str();
    if trimmed.is_empty() || !long_path(path).is_dir() {
        return None;
    }
    let sub = path.join("site");
    if long_path(&sub).is_dir()
        && (long_path(&path.join("report.html")).exists()
            || long_path(&path.join("data")).is_dir()
            || long_path(&path.join("mirror")).is_dir()
            || long_path(&sub.join("index.html")).is_file())
    {
        return Some(sub);
    }
    Some(path.to_path_buf())
}

/// Проверяет, есть ли в каталоге копии хотя бы один файл (чтобы не открывать пустую папку).
pub fn has_files(site_dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(long_path(site_dir)) else {
        return false;
    };
    entries.flatten().next().is_some()
}

/// Запущенный локальный HTTP-сервер копии.
pub struct CopyServer {
    port: u16,
    stop: Arc<AtomicBool>,
    queue: Arc<(Mutex<VecDeque<TcpStream>>, Condvar)>,
    accept_thread: Option<JoinHandle<()>>,
    workers: Vec<JoinHandle<()>>,
}

impl CopyServer {
    /// Запустить сервер на `127.0.0.1:<port>` (`0` — случайный свободный порт).
    pub fn start(root: &Path, port: u16) -> Result<Self, String> {
        if !long_path(root).is_dir() {
            return Err(crate::tr::pick(
                format!("папка копии не найдена: {}", crate::core::show_path(root)),
                format!("copy folder not found: {}", crate::core::show_path(root)),
            ));
        }
        let canon_root = fs::canonicalize(long_path(root)).map_err(|e| e.to_string())?;
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let listener = TcpListener::bind(addr).map_err(|e| {
            crate::tr::pick(
                format!("не удалось занять 127.0.0.1:{port}: {e}"),
                format!("cannot bind 127.0.0.1:{port}: {e}"),
            )
        })?;
        let actual_port = listener.local_addr().map_err(|e| e.to_string())?.port();

        let stop = Arc::new(AtomicBool::new(false));
        let queue: Arc<(Mutex<VecDeque<TcpStream>>, Condvar)> =
            Arc::new((Mutex::new(VecDeque::new()), Condvar::new()));

        let mut workers = Vec::with_capacity(WORKER_COUNT);
        for _ in 0..WORKER_COUNT {
            let stop = Arc::clone(&stop);
            let queue = Arc::clone(&queue);
            let canon_root = canon_root.clone();
            workers.push(thread::spawn(move || {
                loop {
                    let stream = {
                        let (lock, cv) = &*queue;
                        let mut q = lock.lock().unwrap();
                        while q.is_empty() && !stop.load(Ordering::Relaxed) {
                            q = cv.wait(q).unwrap();
                        }
                        if stop.load(Ordering::Relaxed) && q.is_empty() {
                            return;
                        }
                        q.pop_front()
                    };
                    if let Some(stream) = stream {
                        handle_client(stream, &canon_root);
                    }
                }
            }));
        }

        let accept_stop = Arc::clone(&stop);
        let accept_queue = Arc::clone(&queue);
        let accept_thread = thread::spawn(move || {
            for incoming in listener.incoming() {
                if accept_stop.load(Ordering::Relaxed) {
                    break;
                }
                match incoming {
                    Ok(stream) => {
                        let (lock, cv) = &*accept_queue;
                        let mut q = lock.lock().unwrap();
                        if q.len() < 128 {
                            q.push_back(stream);
                            cv.notify_one();
                        }
                    }
                    Err(_) => {
                        if accept_stop.load(Ordering::Relaxed) {
                            break;
                        }
                    }
                }
            }
        });

        Ok(Self {
            port: actual_port,
            stop,
            queue,
            accept_thread: Some(accept_thread),
            workers,
        })
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }

    /// Остановить сервер и освободить порт.
    pub fn stop(&mut self) {
        if self.stop.swap(true, Ordering::Relaxed) {
            return;
        }
        // Будим `listener.incoming()` пустым подключением на петлевой адрес.
        let _ = TcpStream::connect_timeout(
            &SocketAddr::from(([127, 0, 0, 1], self.port)),
            Duration::from_millis(200),
        );
        {
            let (_lock, cv) = &*self.queue;
            cv.notify_all();
        }
        if let Some(h) = self.accept_thread.take() {
            let _ = h.join();
        }
        for h in self.workers.drain(..) {
            let _ = h.join();
        }
    }
}

impl Drop for CopyServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn handle_client(mut stream: TcpStream, canon_root: &Path) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));

    let mut req_buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 2048];
    let header_end = loop {
        match stream.read(&mut tmp) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if req_buf.len() + n > MAX_HEADER_BYTES {
                    let _ = send_status(&mut stream, "431 Request Header Fields Too Large", &[]);
                    return;
                }
                req_buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = find_double_crlf(&req_buf) {
                    break pos;
                }
            }
        }
    };

    let head_text = String::from_utf8_lossy(&req_buf[..header_end]);
    let mut lines = head_text.split("\r\n");
    let Some(request_line) = lines.next() else {
        let _ = send_status(&mut stream, "400 Bad Request", &[]);
        return;
    };
    let mut parts = request_line.split_whitespace();
    let Some(method) = parts.next() else {
        let _ = send_status(&mut stream, "400 Bad Request", &[]);
        return;
    };
    let Some(target) = parts.next() else {
        let _ = send_status(&mut stream, "400 Bad Request", &[]);
        return;
    };

    if method != "GET" && method != "HEAD" {
        let _ = send_status(
            &mut stream,
            "405 Method Not Allowed",
            &[("Allow", "GET, HEAD")],
        );
        return;
    }
    let is_head = method == "HEAD";

    let mut range_hdr: Option<String> = None;
    let mut if_none_match: Option<String> = None;
    for line in lines {
        if let Some((name, val)) = line.split_once(':') {
            let val = val.trim();
            if name.eq_ignore_ascii_case("range") {
                range_hdr = Some(val.to_string());
            } else if name.eq_ignore_ascii_case("if-none-match") {
                if_none_match = Some(val.to_string());
            }
        }
    }

    let Some(file_path) = resolve_target(canon_root, target) else {
        let _ = send_status(&mut stream, "404 Not Found", &[]);
        return;
    };

    let Ok(file) = File::open(&file_path) else {
        let _ = send_status(&mut stream, "404 Not Found", &[]);
        return;
    };
    let Ok(meta) = file.metadata() else {
        let _ = send_status(&mut stream, "404 Not Found", &[]);
        return;
    };
    if !meta.is_file() {
        let _ = send_status(&mut stream, "404 Not Found", &[]);
        return;
    }
    let file_len = meta.len();
    let mtime_secs = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let etag = format!("W/\"{file_len}-{mtime_secs}\"");

    if let Some(tag) = if_none_match.as_deref()
        && etag_matches(tag, &etag)
    {
        let _ = send_status(
            &mut stream,
            "304 Not Modified",
            &[("ETag", &etag), ("Cache-Control", "no-cache")],
        );
        return;
    }

    let ctype = mime_by_ext(&file_path);

    if let Some(range_raw) = range_hdr.as_deref() {
        match parse_range(range_raw, file_len) {
            Ok((start, end)) => {
                let send_len = end - start + 1;
                let cr = format!("bytes {start}-{end}/{file_len}");
                let cl = send_len.to_string();
                let header = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Type: {ctype}\r\nContent-Length: {cl}\r\nContent-Range: {cr}\r\nAccept-Ranges: bytes\r\nETag: {etag}\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"
                );
                if stream.write_all(header.as_bytes()).is_err() || is_head {
                    return;
                }
                let mut reader = BufReader::new(file);
                if reader.seek(SeekFrom::Start(start)).is_err() {
                    return;
                }
                let _ = copy_exact(&mut reader, &mut stream, send_len);
            }
            Err(()) => {
                let cr = format!("bytes */{file_len}");
                let _ = send_status(
                    &mut stream,
                    "416 Range Not Satisfiable",
                    &[("Content-Range", &cr)],
                );
            }
        }
        return;
    }

    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {file_len}\r\nAccept-Ranges: bytes\r\nETag: {etag}\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(header.as_bytes()).is_err() || is_head {
        return;
    }
    let mut reader = BufReader::new(file);
    let _ = copy_exact(&mut reader, &mut stream, file_len);
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn send_status(
    stream: &mut TcpStream,
    status: &str,
    extra: &[(&str, &str)],
) -> std::io::Result<()> {
    let mut out = format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n");
    for (k, v) in extra {
        out.push_str(k);
        out.push_str(": ");
        out.push_str(v);
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    stream.write_all(out.as_bytes())
}

fn copy_exact(
    reader: &mut impl Read,
    writer: &mut impl Write,
    mut left: u64,
) -> std::io::Result<()> {
    let mut buf = [0u8; COPY_BUF_SIZE];
    while left > 0 {
        let want = (buf.len() as u64).min(left) as usize;
        let n = reader.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n])?;
        left -= n as u64;
    }
    Ok(())
}

fn etag_matches(header: &str, current: &str) -> bool {
    header
        .split(',')
        .map(str::trim)
        .any(|tag| tag == "*" || tag == current)
}

/// Разбор заголовка `Range: bytes=A-B` / `bytes=A-` / `bytes=-N`.
/// Мульти-диапазоны не поддерживаются и отклоняются (`416`).
fn parse_range(header: &str, total: u64) -> Result<(u64, u64), ()> {
    if total == 0 {
        return Err(());
    }
    let spec = header.trim();
    let rest = spec.strip_prefix("bytes=").ok_or(())?.trim();
    if rest.contains(',') {
        return Err(());
    }
    let (start_s, end_s) = rest.split_once('-').ok_or(())?;
    let start_s = start_s.trim();
    let end_s = end_s.trim();
    if start_s.is_empty() {
        // Форма `bytes=-N` (последние N байт).
        let suffix: u64 = end_s.parse().map_err(|_| ())?;
        if suffix == 0 {
            return Err(());
        }
        let start = total.saturating_sub(suffix);
        return Ok((start, total - 1));
    }
    let start: u64 = start_s.parse().map_err(|_| ())?;
    if start >= total {
        return Err(());
    }
    let end = if end_s.is_empty() {
        total - 1
    } else {
        let e: u64 = end_s.parse().map_err(|_| ())?;
        if e < start {
            return Err(());
        }
        e.min(total - 1)
    };
    Ok((start, end))
}

/// Разрешение URL в файл внутри `canon_root` правилами ядра (ФТ-137, ФТ-140).
fn resolve_target(canon_root: &Path, raw_target: &str) -> Option<PathBuf> {
    let mut target = raw_target.trim();
    if let Some(rest) = target
        .strip_prefix("http://")
        .or_else(|| target.strip_prefix("https://"))
    {
        target = match rest.find('/') {
            Some(i) => &rest[i..],
            None => "/",
        };
    }
    let target = target.split('#').next().unwrap_or("/");
    let (raw_path, has_query) = match target.split_once('?') {
        Some((p, _)) => (p, true),
        None => (target, false),
    };
    if !raw_path.starts_with('/') || is_unsafe_path(raw_path) {
        return None;
    }

    let url = format!("https://local{target}");
    let rel = paths::url_to_local_path(&url);
    if let Some(found) = check_candidate(canon_root, &rel) {
        return Some(found);
    }
    if has_query {
        let url_no_q = format!("https://local{raw_path}");
        let rel_no_q = paths::url_to_local_path(&url_no_q);
        if let Some(found) = check_candidate(canon_root, &rel_no_q) {
            return Some(found);
        }
    }
    None
}

fn is_unsafe_path(raw_path: &str) -> bool {
    let decoded_full = pct_decode(raw_path);
    if raw_path.contains('\\') || decoded_full.contains('\\') || decoded_full.contains('\0') {
        return true;
    }
    for seg in raw_path.split('/') {
        let dec = pct_decode(seg);
        if seg == ".."
            || dec == ".."
            || dec.contains('\\')
            || dec.contains('/') && dec.contains("..")
        {
            return true;
        }
    }
    false
}

fn check_candidate(canon_root: &Path, rel: &str) -> Option<PathBuf> {
    let candidate = canon_root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
    let canon_file = fs::canonicalize(long_path(&candidate)).ok()?;
    if !canon_file.starts_with(canon_root) {
        return None;
    }
    canon_file.is_file().then_some(canon_file)
}

/// Тип содержимого по расширению **без** `; charset=` (ФТ-138).
fn mime_by_ext(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" => "application/javascript",
        "json" => "application/json",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "eot" => "application/vnd.ms-fontobject",
        "pdf" => "application/pdf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "txt" => "text/plain",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http_get(port: u16, req: &str) -> (String, Vec<u8>) {
        let mut sock = TcpStream::connect(("127.0.0.1", port)).unwrap();
        sock.write_all(req.as_bytes()).unwrap();
        let mut raw = Vec::new();
        sock.read_to_end(&mut raw).unwrap();
        let pos = find_double_crlf(&raw).expect("HTTP headers");
        let head = String::from_utf8_lossy(&raw[..pos]).into_owned();
        let body = raw[pos + 4..].to_vec();
        (head, body)
    }

    #[test]
    fn serves_index_query_params_and_mime_without_charset() {
        let root = std::env::temp_dir().join(format!(
            "sr-serve-basic-{}-{:?}",
            std::process::id(),
            thread::current().id()
        ));
        let _ = fs::remove_dir_all(&root);
        let site = root.join("site");
        fs::create_dir_all(site.join("novosti__q_c49f0e63")).unwrap();
        fs::write(site.join("index.html"), b"<html>home</html>").unwrap();
        let q_rel = paths::url_to_local_path("https://local/novosti?PAGEN_1=2");
        let q_file = site.join(q_rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        fs::create_dir_all(q_file.parent().unwrap()).unwrap();
        fs::write(&q_file, b"<html>page2</html>").unwrap();
        fs::write(site.join("style.css"), b"body{color:#000}").unwrap();

        let resolved = resolve_site_dir(&root).expect("resolve site");
        assert_eq!(resolved, site);

        let mut srv = CopyServer::start(&resolved, 0).unwrap();
        assert!(srv.url().starts_with("http://127.0.0.1:"));

        let (h1, b1) = http_get(srv.port(), "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert!(h1.starts_with("HTTP/1.1 200 OK"), "{h1}");
        assert!(h1.contains("Content-Type: text/html"), "{h1}");
        assert!(!h1.contains("charset="), "без charset (ФТ-138): {h1}");
        assert_eq!(b1, b"<html>home</html>");

        let (h2, b2) = http_get(
            srv.port(),
            "GET /novosti?PAGEN_1=2 HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        assert!(h2.starts_with("HTTP/1.1 200 OK"), "{h2}");
        assert_eq!(b2, b"<html>page2</html>");

        // Fallback без query, если в копии лежит style.css без параметров.
        let (h3, b3) = http_get(
            srv.port(),
            "GET /style.css?ver=123 HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        assert!(h3.starts_with("HTTP/1.1 200 OK"), "{h3}");
        assert!(h3.contains("Content-Type: text/css"), "{h3}");
        assert_eq!(b3, b"body{color:#000}");

        srv.stop();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn enforces_methods_traversal_range_head_and_etag() {
        let root = std::env::temp_dir().join(format!(
            "sr-serve-sec-{}-{:?}",
            std::process::id(),
            thread::current().id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("data.bin"), b"0123456789").unwrap();

        let mut srv = CopyServer::start(&root, 0).unwrap();
        let port = srv.port();

        // POST -> 405
        let (h_post, _) = http_get(port, "POST /data.bin HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert!(
            h_post.starts_with("HTTP/1.1 405 Method Not Allowed"),
            "{h_post}"
        );

        // Traversal & dir without index.html -> 404
        for bad in ["/../etc/passwd", "/%2e%2e/secret", "/sub/"] {
            let (h, _) = http_get(
                port,
                &format!("GET {bad} HTTP/1.1\r\nHost: localhost\r\n\r\n"),
            );
            assert!(h.starts_with("HTTP/1.1 404 Not Found"), "{bad}: {h}");
        }

        // HEAD -> 200 без тела
        let (h_head, b_head) = http_get(port, "HEAD /data.bin HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert!(h_head.starts_with("HTTP/1.1 200 OK"), "{h_head}");
        assert!(h_head.contains("Content-Length: 10"), "{h_head}");
        assert!(b_head.is_empty());

        // Range 206
        let (h_r, b_r) = http_get(
            port,
            "GET /data.bin HTTP/1.1\r\nHost: localhost\r\nRange: bytes=2-5\r\n\r\n",
        );
        assert!(h_r.starts_with("HTTP/1.1 206 Partial Content"), "{h_r}");
        assert!(h_r.contains("Content-Range: bytes 2-5/10"), "{h_r}");
        assert_eq!(b_r, b"2345");

        // Range 416
        let (h_r416, _) = http_get(
            port,
            "GET /data.bin HTTP/1.1\r\nHost: localhost\r\nRange: bytes=50-60\r\n\r\n",
        );
        assert!(
            h_r416.starts_with("HTTP/1.1 416 Range Not Satisfiable"),
            "{h_r416}"
        );
        assert!(h_r416.contains("Content-Range: bytes */10"), "{h_r416}");

        // ETag / 304
        let etag_line = h_head
            .lines()
            .find(|l| l.starts_with("ETag: "))
            .unwrap()
            .trim_start_matches("ETag: ");
        let (h_304, b_304) = http_get(
            port,
            &format!(
                "GET /data.bin HTTP/1.1\r\nHost: localhost\r\nIf-None-Match: {etag_line}\r\n\r\n"
            ),
        );
        assert!(h_304.starts_with("HTTP/1.1 304 Not Modified"), "{h_304}");
        assert!(b_304.is_empty());

        srv.stop();
        // После остановки порт освобождён — соединение должно быть отвергнуто.
        assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
        let _ = fs::remove_dir_all(&root);
    }
}
