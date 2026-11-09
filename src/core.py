"""Core engine of Site Restorer 2.0 — stdlib only.

Key features:
  * all file types downloaded in parallel (interleaved queue)
  * resumable: existing files are never re-downloaded
  * adaptive concurrency (AIMD) driven by real speed and 429 feedback
  * polite rate-limit handling: retry-after, exponential backoff with jitter,
    circuit breaker, ramp-up, keep-alive connections
"""
import gzip as gzipmod
import hashlib
import html as htmlmod
import http.client
import json
import os
import posixpath
import random
import re
import shutil
import socket
import threading
import zlib
import time
import urllib.error
import urllib.parse
import urllib.request
from collections import Counter, defaultdict, deque

UA = "SiteRestorer/2.0 (offline mirror tool; respectful crawler; contact: local user)"
IMG_EXT = {".jpg", ".jpeg", ".png", ".gif", ".webp", ".svg", ".ico", ".bmp"}
CODE_EXT = {".css", ".js", ".json", ".xml", ".woff", ".woff2", ".ttf"}
BAD_CHARS = re.compile(r'[<>:"|?*\x00-\x1f]')
CACHE_RX = re.compile(r"^bitrix/cache/(css|js)/s1/[^/]+/[^/]+/([a-z_]+)_[0-9a-f]+_v1", re.I)
RESIZE_RX = re.compile(r"^upload/resize_cache/iblock/([0-9a-z]+)/[^/]+/(.+)$", re.I)

MAX_THREADS = 24
ERROR_MARKERS = (b"This page is not available",
                 b"doesn't have that page archived",
                 b"Wayback Machine has not archived")
MIN_THREADS = 2


class Cancelled(Exception):
    pass


# ---------------------------------------------------------------- helpers
def _is_error_page(body):
    head = body[:2000]
    return any(m in head for m in ERROR_MARKERS)

META_CHARSET_RX = re.compile(r"""(charset\s*=\s*["']?\s*)([a-zA-Z0-9_.:-]+)""", re.I)
CSS_CHARSET_DECL_RX = re.compile(r"""(@charset\s+["'])([^"']+)(["'])""", re.I)


def _sniff_charset(data):
    """Кодировка, объявленная в документе (meta charset / @charset), или None."""
    head = data[:4096].decode("latin-1", "replace")
    for rx in (META_CHARSET_RX, CSS_CHARSET_DECL_RX):
        m = rx.search(head)
        if m:
            return m.group(2).strip().lower()
    return None


def decode_text(data, declared=None):
    """Байты -> (текст, кодировка).

    Сначала utf-8: он строгий, поэтому корректный utf-8 проходит даже при
    неверно объявленной кодировке. Затем объявленная кодировка, затем cp1251.
    """
    order = ["utf-8"]
    if declared and declared.replace("_", "-") not in ("utf-8", "utf8"):
        order.append(declared)
    order.append("cp1251")
    for enc in order:
        try:
            return data.decode(enc), enc
        except (UnicodeDecodeError, LookupError):
            continue
    return None, None


def force_utf8_charset(text):
    """Документ записан в UTF-8 — приводим объявления кодировки к utf-8."""
    text = META_CHARSET_RX.sub(lambda m: m.group(1) + "utf-8", text)
    text = CSS_CHARSET_DECL_RX.sub(lambda m: m.group(1) + "utf-8" + m.group(3), text)
    return text


def normalize_url(u, host):
    u = (u or "").strip().split("#", 1)[0]
    if u.startswith("//"):
        u = "https:" + u
    if not re.match(r"^[a-zA-Z][a-zA-Z0-9+.-]*://", u):
        u = "https://" + host + "/" + u.lstrip("/")
    p = urllib.parse.urlsplit(u)
    h = (p.hostname or host).lower()
    if h.startswith("www.") and h[4:] == host:
        h = host
    netloc = h
    if p.port and p.port not in (80, 443):
        netloc += ":" + str(p.port)
    return urllib.parse.urlunsplit(("https", netloc, p.path or "/", p.query, ""))


def _safe(seg):
    seg = urllib.parse.unquote(seg)
    seg = BAD_CHARS.sub("_", seg).strip(" .")
    if len(seg) > 80:
        seg = seg[:60] + "_" + hashlib.sha1(seg.encode("utf-8", "replace")).hexdigest()[:8]
    if not seg:
        seg = "_"
    if seg.split(".")[0].lower() in {"con", "prn", "aux", "nul",
                                     "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9",
                                     "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9"}:
        seg = "_" + seg
    return seg


MAX_REL_LEN = 160


def url_to_local_path(url):
    p = urllib.parse.urlsplit(url)
    segs = [_safe(s) for s in p.path.split("/") if s]
    if p.query:
        h = hashlib.sha1(p.query.encode("utf-8", "replace")).hexdigest()[:8]
        if not segs:
            segs = ["index"]
        root, ext = os.path.splitext(segs[-1])
        segs[-1] = "%s__q_%s%s" % (root, h, ext) if ext and len(ext) <= 6 else "%s__q_%s" % (segs[-1], h)
    if not segs:
        return "index.html"
    if p.path.endswith("/") or "." not in segs[-1]:
        segs.append("index.html")
    rel = os.path.join(*segs)
    if len(rel) > MAX_REL_LEN:
        # pathological URLs (embedded data URIs, very deep trees): collapse deterministically
        digest = hashlib.sha1(url.encode("utf-8", "replace")).hexdigest()[:12]
        head = segs[0] if len(segs) > 1 else "_"
        ext = os.path.splitext(segs[-1])[1][:6]
        rel = os.path.join(head, "_long", digest + ext)
    return rel


def file_type(url):
    ext = os.path.splitext(urllib.parse.urlsplit(url).path)[1].lower()
    if ext in IMG_EXT:
        return "img"
    if ext in (".html", ".htm") or ext == "":
        return "html"
    if ext in CODE_EXT:
        return "code"
    return "other"


# ---------------------------------------------------------------- rate
class RateLimiter:
    """Optional global requests-per-second cap (0 = disabled)."""

    def __init__(self, rps):
        self.interval = 1.0 / rps if rps and rps > 0 else 0.0
        self.next_at = 0.0
        self.lock = threading.Lock()

    def wait(self, cancel):
        if self.interval <= 0:
            return
        with self.lock:
            now = time.monotonic()
            slot = max(self.next_at, now)
            self.next_at = slot + self.interval
        delta = slot - time.monotonic()
        while delta > 0:
            if cancel.is_set():
                raise Cancelled()
            time.sleep(min(0.25, delta))
            delta -= 0.25


# ---------------------------------------------------------------- gate
class Gate:
    """Dynamic concurrency limiter (limit can be changed at runtime)."""

    def __init__(self, limit):
        self.cond = threading.Condition()
        self.limit = limit
        self.inflight = 0

    def acquire(self, cancel):
        with self.cond:
            while self.inflight >= self.limit:
                if cancel.is_set():
                    raise Cancelled()
                self.cond.wait(0.2)
            if cancel.is_set():
                raise Cancelled()
            self.inflight += 1

    def release(self):
        with self.cond:
            self.inflight = max(0, self.inflight - 1)
            self.cond.notify()

    def set_limit(self, n):
        with self.cond:
            self.limit = max(1, int(n))
            self.cond.notify_all()


# ---------------------------------------------------------------- http
class HttpClient:
    """Per-thread keep-alive connection with redirects and retry-after support."""

    def __init__(self, host, scheme="http", timeout=30):
        self.host = host
        self.scheme = scheme
        self.timeout = timeout
        self.local = threading.local()
        self.lock = threading.Lock()
        self.consecutive_conn_errors = 0
        self.live = set()

    def _conn(self):
        c = getattr(self.local, "conn", None)
        if c is not None and c.sock is None:
            # соединение закрыто (в т.ч. сервером по keep-alive) — открываем новое
            self.close()
            c = None
        if c is None:
            cls = http.client.HTTPSConnection if self.scheme == "https" else http.client.HTTPConnection
            c = cls(self.host, timeout=self.timeout)
            self.local.conn = c
            with self.lock:
                self.live.add(c)
        return c

    def close(self):
        c = getattr(self.local, "conn", None)
        if c is not None:
            try:
                c.close()
            except Exception:
                pass
            with self.lock:
                self.live.discard(c)
            self.local.conn = None

    def close_all(self):
        with self.lock:
            conns = list(self.live)
            self.live.clear()
        for c in conns:
            try:
                c.close()
            except Exception:
                pass

    def get(self, path, timeout=None):
        """GET path (e.g. '/web/...'); returns (status, body, retry_after)."""
        attempts = 0
        redirects = 0
        while attempts < 3:
            attempts += 1
            try:
                conn = self._conn()
                conn.request("GET", path, headers={
                    "User-Agent": UA, "Accept": "*/*", "Accept-Encoding": "identity",
                    "Connection": "keep-alive",
                })
                resp = conn.getresponse()
                body = resp.read()
                status = resp.status
                headers = dict((k.lower(), v) for k, v in resp.getheaders())
                enc = (headers.get("content-encoding") or "").lower()
                if enc == "gzip":
                    try:
                        body = gzipmod.decompress(body)
                    except OSError:
                        pass
                elif enc == "deflate":
                    try:
                        body = zlib.decompress(body)
                    except zlib.error:
                        try:
                            body = zlib.decompress(body, -15)
                        except zlib.error:
                            pass
                with self.lock:
                    self.consecutive_conn_errors = 0
                if status in (301, 302, 303, 307, 308) and redirects < 3:
                    loc = headers.get("location", "")
                    redirects += 1
                    if loc.startswith("http"):
                        p = urllib.parse.urlsplit(loc)
                        if p.scheme != self.scheme:
                            self.scheme = p.scheme
                            self.close()
                        path = p.path + ("?" + p.query if p.query else "")
                    continue
                ra = headers.get("retry-after")
                try:
                    ra = float(ra) if ra else None
                except ValueError:
                    ra = None
                return status, body, ra
            except Exception as e:  # noqa: network, protocol, closed-socket races
                self.close()
                with self.lock:
                    self.consecutive_conn_errors += 1
                    n = self.consecutive_conn_errors
                if n >= 3 and self.scheme == "https":
                    with self.lock:
                        self.scheme = "http"
                if attempts >= 3:
                    return 0, b"", None
                time.sleep(0.5 * attempts)
        return 0, b"", None


# ---------------------------------------------------------------- engine
class Engine:
    def __init__(self, site, outdir, workers=MAX_THREADS, rate=0.0, emit=None, cancel=None,
                 limit=0, lang="ru"):
        from sanitize import sanitize_site
        host, note = sanitize_site(site)
        if not host:
            raise ValueError("invalid_site")
        self.host = host
        self.site = "https://" + host
        self.outdir = outdir
        self.max_threads = max(MIN_THREADS, int(workers))
        self.emit = emit or (lambda **kw: None)
        self.cancel = cancel or threading.Event()
        self.limit = int(limit or 0)
        self.lang = lang if lang in ("ru", "en") else "ru"
        self.mirror = os.path.join(outdir, "mirror")
        self.build_dir = os.path.join(outdir, "site")
        self.data_dir = os.path.join(outdir, "data")
        self.items = []
        self.counters = Counter()
        self.types = defaultdict(lambda: [0, 0])
        self.total_bytes = 0
        self.lock = threading.Lock()
        self.t_start = time.time()
        self.last_emit = 0.0
        self._by_basename = {}
        self._by_kind = {}
        self._relpaths = set()

        # adaptive control
        self.gate = Gate(MIN_THREADS)
        self.speed_window = deque(maxlen=20)
        self.win_bytes = 0
        self.win_files = 0
        self.win_429 = 0
        self.win_t0 = time.time()
        self.throttle = 0.0          # global pause requested by circuit breaker
        self.client = HttpClient("web.archive.org", scheme="http", timeout=60)
        self.limiter = RateLimiter(rate)
        self._inv_failed = False

    def _sleep(self, seconds):
        end = time.monotonic() + max(0.0, seconds)
        while time.monotonic() < end:
            if self.cancel.is_set():
                raise Cancelled()
            time.sleep(min(0.25, max(0.0, end - time.monotonic())))

    # ---------- logging / progress
    def _t(self, ru, en):
        return ru if getattr(self, "lang", "ru") == "ru" else en

    def _log(self, text, level="info"):
        self.emit(kind="log", text=text, level=level)

    def _snapshot(self, stage, overall, text):
        done = self.counters["done"]
        total = self.counters["total"]
        elapsed = max(0.001, time.time() - self.t_start)
        speed = done / elapsed if elapsed else 0
        eta = int((total - done) / speed) if speed > 0.05 and total > done else 0
        self.emit(kind="progress", stage=stage, overall=overall, text=text,
                  done=done, total=total, ok=self.counters["ok"],
                  missing=self.counters["missing"], errors=self.counters["errors"],
                  bytes=self.total_bytes, speed=speed, eta=eta,
                  bps=self.counters.get("bps") or (self.total_bytes / max(0.5, elapsed)),
                  threads=self.gate.limit,
                  types={k: list(v) for k, v in self.types.items()},
                  pause=int(max(0, self.throttle - time.time())))

    def _progress(self, stage, overall, text="", force=False):
        now = time.time()
        if not force and now - self.last_emit < 0.3:
            return
        self.last_emit = now
        self._snapshot(stage, overall, text)

    def _check_cancel(self):
        if self.cancel.is_set():
            raise Cancelled()

    # ---------- stage 1: inventory
    def _beat(self, ru, en, stop, t0):
        while not stop.wait(5.0):
            el = time.time() - t0
            self._log(self._t("%s… %.0f с" % (ru, el), "%s… %.0f s" % (en, el)))

    def _fmt_sec(self, v):
        return ("%.1f" % v).replace(".", ",") if self.lang == "ru" else "%.1f" % v

    def _fmt_size(self, n):
        if self.lang == "ru":
            if n < 1024:
                return "%d Б" % n
            if n < 1024 * 1024:
                return ("%.1f КБ" % (n / 1024.0)).replace(".", ",")
            return ("%.2f МБ" % (n / 1048576.0)).replace(".", ",")
        if n < 1024:
            return "%d B" % n
        if n < 1024 * 1024:
            return "%.1f KB" % (n / 1024.0)
        return "%.2f MB" % (n / 1048576.0)

    def _get_streaming(self, url, timeout, label_ru, label_en, step_ru, step_en, step_key):
        """GET with live byte/record counters (CDX answers are large)."""
        short = url if len(url) < 150 else url[:147] + "..."
        self._log(self._t("%s → GET %s" % (label_ru, short), "%s → GET %s" % (label_en, short)))
        self.emit(kind="step", stage="inventory", key=step_key,
                  title=self._t(step_ru, step_en),
                  text=self._t("запрос отправлен, ждём первые данные",
                               "request sent, waiting for the first bytes"))
        req = urllib.request.Request(url, headers={"User-Agent": UA, "Accept": "*/*",
                                                   "Accept-Encoding": "identity"})
        t0 = time.time()
        buf = []
        got = 0
        lines = 0
        total = 0
        last_emit = 0.0
        last_log = t0
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                status = r.status
                try:
                    total = int(r.headers.get("Content-Length") or 0)
                except (TypeError, ValueError):
                    total = 0
                while True:
                    if self.cancel.is_set():
                        raise Cancelled()
                    chunk = r.read(65536)
                    if not chunk:
                        break
                    buf.append(chunk)
                    got += len(chunk)
                    lines += chunk.count(b"\n")
                    now = time.time()
                    if now - last_emit > 0.4:
                        last_emit = now
                        self.emit(kind="step", stage="inventory", key=step_key,
                                  title=self._t(step_ru, step_en),
                                  text=self._live_text(step_ru, step_en, got, total, lines))
                    if now - last_log > 5.0:
                        last_log = now
                        self._log(self._t("%s (получение)… %s%s, записей ~%d, %.0f с" % (
                                              label_ru, self._fmt_size(got),
                                              (" из " + self._fmt_size(total)) if total else "",
                                              max(0, lines - 2), now - t0),
                                          "%s (receiving)… %s%s, records ~%d, %.0f s" % (
                                              label_en, self._fmt_size(got),
                                              (" of " + self._fmt_size(total)) if total else "",
                                              max(0, lines - 2), now - t0)))
            body = b"".join(buf)
            el = time.time() - t0
            self._log(self._t("%s ← HTTP %s, %s, записей ~%d, %s с" % (
                                  label_ru, status, self._fmt_size(len(body)),
                                  max(0, lines - 2), self._fmt_sec(el)),
                              "%s ← HTTP %s, %s, records ~%d, %s s" % (
                                  label_en, status, self._fmt_size(len(body)),
                                  max(0, lines - 2), self._fmt_sec(el))))
            return status, body, None
        except urllib.error.HTTPError as e:
            el = time.time() - t0
            ra = e.headers.get("Retry-After") if e.headers else None
            try:
                ra = float(ra) if ra else None
            except ValueError:
                ra = None
            self._log(self._t("%s ← HTTP %s, %s с" % (label_ru, e.code, self._fmt_sec(el)),
                              "%s ← HTTP %s, %s s" % (label_en, e.code, self._fmt_sec(el))), "warn")
            return e.code, b"", ra
        except Cancelled:
            raise
        except Exception as e:
            self._log(self._t("%s ← ошибка: %s" % (label_ru, e), "%s ← error: %s" % (label_en, e)), "warn")
            return 0, b"", None

    def _live_text(self, step_ru, step_en, got, total, lines):
        if self.lang == "ru":
            size = ("%s из %s" % (self._fmt_size(got), self._fmt_size(total))
                    if total else self._fmt_size(got))
            return "%s · записей ~%d" % (size, max(0, lines - 2))
        size = ("%s of %s" % (self._fmt_size(got), self._fmt_size(total))
                if total else self._fmt_size(got))
        return "%s · records ~%d" % (size, max(0, lines - 2))

    def _get_logged(self, url, timeout, label_ru, label_en, step_ru=None, step_en=None):
        if step_ru:
            self.emit(kind="step", stage="inventory", key=step_ru,
                      title=self._t(step_ru, step_en or step_ru), text="")
        short = url if len(url) < 150 else url[:147] + "..."
        self._log(self._t("%s → GET %s" % (label_ru, short), "%s → GET %s" % (label_en, short)))
        stop = threading.Event()
        t0 = time.time()
        th = threading.Thread(target=self._beat, args=(label_ru + " (получение)", label_en + " (receiving)",
                                                       stop, t0), daemon=True)
        th.start()
        try:
            status, body, ra = self._simple_get(url, timeout=timeout)
        finally:
            stop.set()
        el = time.time() - t0
        size = len(body)
        self._log(self._t("%s ← HTTP %s, %s, %s с" % (label_ru, status or "нет ответа", self._fmt_size(size), self._fmt_sec(el)),
                          "%s ← HTTP %s, %s, %s s" % (label_en, status or "no answer", self._fmt_size(size), self._fmt_sec(el))),
                  "warn" if status != 200 else "info")
        return status, body, ra

    def _paged_inventory(self, page_limit, max_pages):
        base = "http://web.archive.org/cdx/search/cdx"
        rows = []
        for page in range(max_pages):
            self._check_cancel()
            q = urllib.parse.urlencode({
                "url": self.host, "matchType": "domain", "output": "json",
                "fl": "timestamp,original,mimetype,statuscode",
                "filter": "statuscode:200", "collapse": "urlkey",
                "limit": str(page_limit), "page": str(page),
            })
            status, body, ra = (0, b"", None)
            for attempt in range(3):
                status, body, ra = self._get_streaming(
                    base + "?" + q, 120,
                    "CDX страница %d" % page, "CDX page %d" % page,
                    "получение адресов из индекса · страница %d" % (page + 1),
                    "fetching URLs from the index · page %d" % (page + 1),
                    "page-%d" % page)
                if status == 200:
                    break
                if status in (429, 500, 502, 503, 504):
                    wait = ra or (10 + attempt * 15)
                    self._log(self._t("CDX временно недоступен (HTTP %s) — пауза %.0f с" % (status, wait),
                                      "CDX temporarily unavailable (HTTP %s) — pause %.0f s" % (status, wait)), "warn")
                    self._sleep(wait)
                    continue
                break
            if status != 200:
                self._inv_failed = True
                break
            try:
                chunk = json.loads(body.decode("utf-8", "replace")) if body.strip() else []
            except Exception:
                self._log(self._t("CDX: ответ не разобран", "CDX: response not parsed"), "warn")
                break
            if chunk and chunk[0] and chunk[0][0] == "timestamp":
                chunk = chunk[1:]
            rows.extend(chunk)
            self._progress("inventory", 2 + 5.0 * (page + 1) / max(1, max_pages),
                           "%d / %d" % (page + 1, max_pages), force=True)
            if len(chunk) < page_limit:
                break
        return rows

    def inventory(self):
        self._progress("inventory", 2, "", force=True)
        self._log(self._t("Индекс архива: %s" % self.host, "Archive index: %s" % self.host))
        base = "http://web.archive.org/cdx/search/cdx"

        probe_url = base + "?" + urllib.parse.urlencode(
            {"url": self.host, "matchType": "domain", "showNumPages": "true", "output": "json"})
        status, body, _ra = self._get_logged(
            probe_url, 90, "CDX: оценка размера", "CDX: size probe",
            "оценка объёма архива", "estimating archive size")
        pages = 1
        txt = body.decode("utf-8", "replace").strip()
        if status == 200 and txt:
            try:
                info = json.loads(txt)
                if isinstance(info, dict):
                    pages = max(1, int(info.get("pages", 1)))
                elif isinstance(info, list):
                    for row in info:
                        if isinstance(row, list) and len(row) >= 2 and \
                                str(row[0]).strip().lower() in ("numpages", "pages"):
                            pages = max(1, int(row[1]))
                            break
            except Exception:
                if txt.isdigit():
                    pages = max(1, int(txt))
        self._log(self._t("CDX: оценка страниц — %d" % pages, "CDX: pages reported — %d" % pages))

        rows = []
        if pages <= 1:
            q = urllib.parse.urlencode({
                "url": self.host, "matchType": "domain", "output": "json",
                "fl": "timestamp,original,mimetype,statuscode",
                "filter": "statuscode:200", "collapse": "urlkey", "limit": "50000",
            })
            status, body, ra = self._get_streaming(
                base + "?" + q, 120, "CDX: запрос списка", "CDX: list request",
                "получение списка адресов из индекса", "fetching the URL list from the index",
                "list")
            if status == 200 and body.strip().startswith(b"["):
                try:
                    chunk = json.loads(body.decode("utf-8", "replace"))
                    if chunk and chunk[0] and chunk[0][0] == "timestamp":
                        chunk = chunk[1:]
                    rows = chunk
                except Exception:
                    self._log(self._t("CDX: JSON не разобран", "CDX: JSON parse failed"), "warn")
            if not rows:
                self._log(self._t("Быстрый запрос не дал результата — перехожу на постраничный обход",
                                  "Fast request returned nothing — switching to paged crawl"), "warn")
                rows = self._paged_inventory(5000, 200)
        else:
            rows = self._paged_inventory(20000, min(pages, 200))

        if pages > 200:
            self._log(self._t("CDX: взято %d из %d страниц (лимит)" % (200, pages),
                              "CDX: fetched %d of %d pages (cap)" % (200, pages)), "warn")

        self._log(self._t("CDX: записей получено %d" % len(rows), "CDX: records received %d" % len(rows)))
        if len(rows) >= 50000:
            self._log(self._t("Достигнут лимит 50 000 адресов — сайт очень большой, копия будет неполной",
                              "50,000 URL cap reached — the site is very large, the copy will be partial"), "warn")
        best = {}
        for row in rows:
            try:
                ts, orig, mime = row[0], row[1], row[2]
            except Exception:
                continue
            u = normalize_url(orig, self.host)
            if u not in best or ts > best[u][0]:
                best[u] = (ts, orig, mime)
        self.items = [{"url": u, "ts": v[0], "orig": v[1], "mime": v[2],
                       "path": url_to_local_path(u), "type": file_type(u)}
                      for u, v in best.items()]
        if self.limit:
            self.items = self.items[: self.limit]
        for it in self.items:
            self.types[it["type"]][1] += 1
        os.makedirs(self.data_dir, exist_ok=True)
        with open(os.path.join(self.data_dir, "inventory.jsonl"), "w", encoding="utf-8") as f:
            for it in self.items:
                f.write(json.dumps(it, ensure_ascii=False) + "\n")
        if not self.items:
            if self._inv_failed:
                raise RuntimeError("archive unavailable")
            raise RuntimeError("archive has no pages for this address")
        self._log(self._t("Адресов найдено: %d" % len(self.items), "URLs found: %d" % len(self.items)))
        self._progress("inventory", 8, "", force=True)
        return self.items

    def _simple_get(self, url, timeout=60):
        req = urllib.request.Request(url, headers={"User-Agent": UA, "Accept": "*/*"})
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                return r.status, r.read(), None
        except urllib.error.HTTPError as e:
            ra = e.headers.get("Retry-After") if e.headers else None
            try:
                ra = float(ra) if ra else None
            except ValueError:
                ra = None
            return e.code, b"", ra
        except Exception:
            return 0, b"", None

    # ---------- stage 2: download
    def _order_interleaved(self, items):
        buckets = defaultdict(list)
        for it in items:
            buckets[it["type"]].append(it)
        for k in buckets:
            buckets[k].sort(key=lambda i: i["url"])
        out = []
        idx = 0
        order = ["html", "code", "img", "other"]
        while True:
            added = False
            for k in order:
                b = buckets.get(k) or []
                if idx < len(b):
                    out.append(b[idx])
                    added = True
            if not added:
                break
            idx += 1
        return out

    def _fetch_one(self, item):
        url, path = item["url"], item["path"]
        full = os.path.join(self.mirror, path)
        if os.path.isfile(full) and os.path.getsize(full) > 0:
            with self.lock:
                self.counters["done"] += 1
                self.counters["ok"] += 1
                self.types[item["type"]][0] += 1
            return

        tries = ["/web/%sid_/%s" % (item["ts"], item["url"])]
        year = item["ts"][:4]
        for y in (str(time.gmtime().tm_year), str(time.gmtime().tm_year - 1), str(time.gmtime().tm_year - 2)):
            if y != year:
                tries.append("/web/%sid_/%s" % (y, item["url"]))

        ok = False
        missing = False
        size = 0
        for path_q in tries:
            self._check_cancel()
            for attempt in range(2):
                while self.throttle > time.time():
                    self._sleep(0.5)
                self.limiter.wait(self.cancel)
                status, body, ra = self.client.get(path_q)
                if status == 200 and len(body) > 20 and not _is_error_page(body):
                    os.makedirs(os.path.dirname(full) or ".", exist_ok=True)
                    tmp = full + ".part"
                    try:
                        with open(tmp, "wb") as f:
                            f.write(body)
                        os.replace(tmp, full)
                    except OSError:
                        with open(full, "wb") as f:
                            f.write(body)
                    ok = True
                    size = len(body)
                    break
                if status == 429:
                    with self.lock:
                        self.win_429 += 1
                    wait = ra if ra else min(120, 20 * (attempt + 1)) + random.uniform(0, 5)
                    self._log(self._t("429: пауза %.0f с" % wait, "429: pause %.0f s" % wait), "warn")
                    self._sleep(wait)
                    continue
                if status == 404:
                    missing = True
                    break
                if status == 0:
                    time.sleep(1 + attempt)
                    continue
                break
            if ok:
                break

        with self.lock:
            self.counters["done"] += 1
            if ok:
                self.counters["ok"] += 1
                self.total_bytes += size
                self.win_bytes += size
                self.win_files += 1
                self.types[item["type"]][0] += 1
            elif missing:
                self.counters["missing"] += 1
            else:
                self.counters["errors"] += 1

    def _controller(self, stop_event):
        """AIMD: raise concurrency while speed grows and no 429; cut on trouble."""
        prev_speed = 0.0
        bad_streak = 0
        while not stop_event.is_set():
            time.sleep(3.0)
            with self.lock:
                dt = max(0.001, time.time() - self.win_t0)
                bps = self.win_bytes / dt
                files = self.win_files
                n429 = self.win_429
                self.win_bytes = self.win_files = self.win_429 = 0
                self.win_t0 = time.time()
                self.counters["bps"] = bps
            limit = self.gate.limit
            if n429:
                bad_streak += 1
                new = max(MIN_THREADS, limit // 2)
                if bad_streak >= 3:
                    self.throttle = time.time() + 120
                    new = MIN_THREADS
                    bad_streak = 0
                    self._log(self._t("Много 429 — пауза 120 с, потоков: %d" % new, "Many 429 — 120 s pause, threads: %d" % new), "warn")
                else:
                    self._log(self._t("429 — снижаю потоки: %d → %d" % (limit, new), "429 — lowering threads: %d → %d" % (limit, new)), "warn")
                self.gate.set_limit(new)
            elif files > 0 and bps >= prev_speed * 0.95:
                if limit < self.max_threads:
                    self.gate.set_limit(limit + 1)
            elif files > 0 and bps < prev_speed * 0.6 and limit > MIN_THREADS:
                self.gate.set_limit(max(MIN_THREADS, limit - 1))
            prev_speed = bps

    def download(self):
        todo = [it for it in self.items
                if not (os.path.isfile(os.path.join(self.mirror, it["path"]))
                        and os.path.getsize(os.path.join(self.mirror, it["path"])) > 0)]
        already = len(self.items) - len(todo)
        todo_paths = {it["path"] for it in todo}
        self.counters["total"] = len(self.items)
        self.counters["done"] = already
        self.counters["ok"] = already
        for it in self.items:
            if it["path"] not in todo_paths:
                self.types[it["type"]][0] += 1
        self._log(self._t("Уже в папке: %d, к скачиванию: %d" % (already, len(todo)), "Already present: %d, to download: %d" % (already, len(todo))))
        self.t_start = time.time()

        queue = self._order_interleaved(todo)
        qlock = threading.Lock()
        pos = [0]
        stop = threading.Event()

        def worker():
            while True:
                if self.cancel.is_set():
                    return
                with qlock:
                    if pos[0] >= len(queue):
                        return
                    item = queue[pos[0]]
                    pos[0] += 1
                try:
                    self.gate.acquire(self.cancel)
                    self._fetch_one(item)
                except Cancelled:
                    return
                except Exception as e:  # noqa
                    with self.lock:
                        self.counters["errors"] += 1
                    msg = str(e)
                    if "WinError 206" in msg or "too long" in msg.lower() or "слишком" in msg:
                        msg = self._t("слишком длинный путь — файл пропущен",
                                      "path too long — file skipped")
                    self._log(self._t("Ошибка: %s" % msg, "Error: %s" % msg), "err")
                finally:
                    self.gate.release()
                total = self.counters["total"]
                done_n = self.counters["done"]
                self._progress("download", 10 + 75.0 * done_n / max(1, total),
                               self._t("Скачано %d из %d" % (done_n, total),
                                       "Downloaded %d of %d" % (done_n, total)))

        ctl = threading.Thread(target=self._controller, args=(stop,), daemon=True)
        ctl.start()

        def cancel_watch():
            while not stop.is_set():
                if self.cancel.wait(0.2):
                    self.client.close_all()
                    return

        threading.Thread(target=cancel_watch, daemon=True).start()
        threads = [threading.Thread(target=worker, daemon=True) for _ in range(self.max_threads)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        stop.set()
        self._progress("download", 85, "", force=True)

    # ---------- stage 3: build
    def _index_mirror(self):
        self._relpaths.clear()
        self._by_basename.clear()
        self._by_kind.clear()
        for dp, _dn, fn in os.walk(self.mirror):
            for f in fn:
                if f.endswith(".part"):
                    continue
                rel = os.path.relpath(os.path.join(dp, f), self.mirror).replace("\\", "/")
                self._relpaths.add(rel.lower())
                self._by_basename.setdefault(f.lower(), rel)
                m = CACHE_RX.match(rel)
                if m:
                    self._by_kind.setdefault((m.group(1), m.group(2).lower()), rel)

    def _resolve(self, rel):
        rel = rel.replace("\\", "/").lstrip("/")
        if rel.lower() in self._relpaths:
            return rel
        m = CACHE_RX.match(rel)
        if m:
            alt = self._by_kind.get((m.group(1), m.group(2).lower()))
            if alt:
                return alt
        m = RESIZE_RX.match(rel)
        if m:
            orig = "upload/iblock/%s/%s" % (m.group(1), m.group(2))
            if orig.lower() in self._relpaths:
                return orig
        alt = self._by_basename.get(posixpath.basename(rel).lower())
        if alt and alt.count("/") == rel.count("/"):
            return alt
        return None

    def _to_local(self, url):
        u = normalize_url(htmlmod.unescape(url), self.host)
        got = self._resolve(url_to_local_path(u))
        return "/" + got if got else None

    def build(self):
        self._index_mirror()
        if os.path.isdir(self.build_dir):
            shutil.rmtree(self.build_dir)
        os.makedirs(self.build_dir, exist_ok=True)
        files = []
        for dp, _dn, fn in os.walk(self.mirror):
            for f in fn:
                if not f.endswith(".part"):
                    files.append(os.path.join(dp, f))
        total = max(1, len(files))
        site_rx = re.compile(r"(?:https?:)?//(?:www\.)?" + re.escape(self.host) + r"(?::\d+)?(/[^\"'\s<>()\\]*)", re.I)
        attr_rx = re.compile(r'(?P<a>\b(?:href|src|data-src|data-bg|poster|action|data-original))'
                             r'\s*=\s*(?:"(?P<dq>[^"]*)"|\'(?P<sq>[^\']*)\'|(?P<uq>[^\s"\'<>`=]+))',
                             re.I)
        srcset_rx = re.compile(r'(?P<a>\bsrcset|\bdata-srcset)\s*=\s*(?P<q>["\'])(?P<v>.*?)(?P=q)', re.I | re.S)
        css_rx = re.compile(r'url\(\s*(["\']?)([^"\')]+)\1\s*\)', re.I)
        base_rx = re.compile(r"<base\b[^>]*>", re.I)

        def rel_from(src_dir, target):
            tgt = target.lstrip("/")
            if not tgt or tgt.endswith("/"):
                tgt = (tgt + "index.html") if tgt else "index.html"
            out = posixpath.relpath(tgt, src_dir or ".")
            if target.endswith("/") and out.endswith("/index.html"):
                out = out[:-len("index.html")]
            return out or "."

        def rw(val, src_dir):
            raw = val.strip()
            if not raw or raw.startswith(("#", "mailto:", "tel:", "javascript:", "data:")):
                return val
            if site_rx.match(raw):
                tgt = self._to_local(raw)
                return rel_from(src_dir, tgt) if tgt else val
            if raw.startswith("/") and not raw.startswith("//"):
                tgt = self._to_local("https://" + self.host + raw)
                return rel_from(src_dir, tgt) if tgt else val
            return val

        for i, src in enumerate(files, 1):
            self._check_cancel()
            rel = os.path.relpath(src, self.mirror)
            dst = os.path.join(self.build_dir, rel)
            os.makedirs(os.path.dirname(dst) or ".", exist_ok=True)
            low = src.lower()
            rel_posix = rel.replace("\\", "/")
            src_dir = posixpath.dirname(rel_posix)
            if low.endswith((".html", ".htm", ".css", ".js")):
                with open(src, "rb") as fh:
                    data = fh.read()
                text, _codec = decode_text(data, _sniff_charset(data))
                if text is None:
                    shutil.copy2(src, dst)
                elif low.endswith(".js"):
                    text = re.sub(r"https?://(?:www\.)?" + re.escape(self.host) + r"(?::\d+)?/", "/", text)
                    with open(dst, "w", encoding="utf-8", newline="") as fh:
                        fh.write(text)
                else:
                    def a_sub(m):
                        # значение может быть в двойных, одинарных кавычках или без кавычек
                        if m.group("dq") is not None:
                            val, quote = m.group("dq"), '"'
                        elif m.group("sq") is not None:
                            val, quote = m.group("sq"), "'"
                        else:
                            val, quote = m.group("uq"), '"'
                        return '%s=%s%s%s' % (m.group("a"), quote, rw(val, src_dir), quote)
                    text = attr_rx.sub(a_sub, text)

                    def s_sub(m):
                        parts = []
                        for item in m.group("v").split(","):
                            item = item.strip()
                            if item:
                                bits = item.split()
                                bits[0] = rw(bits[0], src_dir)
                                parts.append(" ".join(bits))
                        return '%s=%s%s%s' % (m.group("a"), m.group("q"), ", ".join(parts), m.group("q"))
                    text = srcset_rx.sub(s_sub, text)

                    def c_sub(m):
                        return "url(%s%s%s)" % (m.group(1), rw(m.group(2), src_dir), m.group(1))
                    text = css_rx.sub(c_sub, text)
                    text = base_rx.sub("", text)
                    text = force_utf8_charset(text)
                    with open(dst, "w", encoding="utf-8", newline="") as fh:
                        fh.write(text)
            else:
                shutil.copy2(src, dst)
            if i % 25 == 0 or i == total:
                self._progress("build", 86 + 9.0 * i / total, "%d / %d" % (i, total), force=(i == total))
        self._log(self._t("Собрано файлов: %d" % total, "Files assembled: %d" % total))
        return total

    # ---------- stage 4: verify
    def verify(self):
        ref_rx = re.compile(r'(?:href|src|data-src|poster|action)=(?:"([^"#]+)"|\'([^\'#]+)\')', re.I)
        pages = refs = broken = 0
        for dp, _dn, fn in os.walk(self.build_dir):
            for f in fn:
                if not f.lower().endswith((".html", ".htm")):
                    continue
                pages += 1
                with open(os.path.join(dp, f), encoding="utf-8", errors="replace") as fh:
                    txt = fh.read()
                for m in ref_rx.finditer(txt):
                    u = (m.group(1) or m.group(2) or "").strip()
                    if not u or u.startswith(("http://", "https://", "//", "mailto:", "tel:",
                                              "javascript:", "data:")):
                        continue
                    refs += 1
                    target = os.path.normpath(os.path.join(dp, urllib.parse.unquote(
                        urllib.parse.urlsplit(u).path).replace("/", os.sep)))
                    if os.path.isdir(target):
                        target = os.path.join(target, "index.html")
                    if not os.path.exists(target):
                        broken += 1
                if pages % 50 == 0:
                    self._progress("verify", 95 + 3.0 * min(pages, 500) / 500, str(pages))
        self._progress("verify", 98, str(pages), force=True)
        return {"pages": pages, "refs": refs, "broken": broken}

    # ---------- stage 5: report
    def report(self, stats):
        rows = []
        for name, key in (("HTML", "html"), ("CSS/JS/XML", "code"), ("IMG", "img"), ("OTHER", "other")):
            ok, tot = self.types.get(key, [0, 0])
            rows.append("<tr><td>%s</td><td class='n'>%d</td><td class='n'>%d</td><td class='n'>%.0f%%</td></tr>"
                        % (name, ok, tot, 100.0 * ok / tot if tot else 0))
        ok = self.counters["ok"]
        total = self.counters["total"]
        values = {
            "host": htmlmod.escape(self.host), "date": time.strftime("%Y-%m-%d %H:%M"),
            "ok": str(ok), "total": str(total),
            "pct": "%.1f" % (100.0 * ok / max(1, total)),
            "mb": "%.0f" % (self.total_bytes / 1e6),
            "missing": str(self.counters["missing"]), "errors": str(self.counters["errors"]),
            "pages": str(stats["pages"]), "refs": str(stats["refs"]), "broken": str(stats["broken"]),
            "rows": "\n".join(rows),
        }
        doc = REPORT_TMPL
        for k, v in values.items():
            doc = doc.replace("{{%s}}" % k, v)
        path = os.path.join(self.outdir, "report.html")
        with open(path, "w", encoding="utf-8") as fh:
            fh.write(doc)
        self._progress("report", 100, "", force=True)
        return path

    def run(self):
        self.inventory()
        self._check_cancel()
        self.download()
        self._check_cancel()
        self.build()
        self._check_cancel()
        stats = self.verify()
        path = self.report(stats)
        try:
            self.client.close()
        except Exception:
            pass
        return {"ok": self.counters["ok"], "total": self.counters["total"],
                "report": path, "site": self.build_dir, "stats": stats}


REPORT_TMPL = """<!DOCTYPE html><html lang="en"><head><meta charset="utf-8">
<title>Mirror report: {{host}}</title>
<style>body{font:15px/1.5 "Segoe UI",Arial;background:#f7f8fa;color:#16181d;margin:0}
.w{max-width:860px;margin:0 auto;padding:36px 22px}h1{font-size:26px;margin:0 0 4px}
.s{color:#6b7280;margin-bottom:24px}.g{display:grid;grid-template-columns:repeat(auto-fit,minmax(160px,1fr));gap:12px}
.c{background:#fff;border:1px solid #e5e7eb;border-radius:10px;padding:14px 16px}
.k{font-size:12px;text-transform:uppercase;letter-spacing:.07em;color:#6b7280}
.v{font-size:24px;font-weight:650}table{width:100%;border-collapse:collapse;background:#fff;border:1px solid #e5e7eb;border-radius:10px;margin-top:14px}
th,td{padding:9px 12px;border-bottom:1px solid #eee;text-align:left}th{background:#fafbfc}
.n{text-align:right;font-variant-numeric:tabular-nums}h2{font-size:18px;margin:28px 0 8px}
</style></head><body><div class="w">
<h1>Mirror report: {{host}}</h1><div class="s">Generated {{date}}</div>
<div class="g"><div class="c"><div class="k">Downloaded</div><div class="v">{{ok}} / {{total}}</div></div>
<div class="c"><div class="k">Coverage</div><div class="v">{{pct}}%</div></div>
<div class="c"><div class="k">Size</div><div class="v">{{mb}} MB</div></div>
<div class="c"><div class="k">Pages</div><div class="v">{{pages}}</div></div></div>
<h2>By file type</h2><table><tr><th>Type</th><th class="n">Downloaded</th><th class="n">Total</th><th class="n">Share</th></tr>{{rows}}</table>
<h2>Integrity</h2><table>
<tr><td>Internal links</td><td class="n">{{refs}}</td></tr>
<tr><td>Broken links</td><td class="n">{{broken}}</td></tr>
<tr><td>Not in archive</td><td class="n">{{missing}}</td></tr>
<tr><td>Download errors</td><td class="n">{{errors}}</td></tr></table>
<p style="color:#6b7280;margin-top:24px">Static copy: cart, login and AJAX parts of the original site do not work.
Serve the <b>site</b> folder with any local web server.</p>
</div></body></html>"""
