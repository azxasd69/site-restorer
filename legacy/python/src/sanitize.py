"""Robust parsing of whatever a user pastes into the "site address" field.

Handles:
  * plain host, with/without scheme, path, query, port, trailing dot
  * text around the address, quotes, brackets, newlines, NBSP, zero-width chars
  * URLs copied from Wayback / Google / Yandex redirects
  * uppercase, mixed case
  * Cyrillic homoglyphs and text typed in the Russian keyboard layout
  * real Cyrillic (IDN) domains -> punycode
"""
import re
import unicodedata
import urllib.parse

# ЙЦУКЕН -> QWERTY (physical key mapping)
RU_LAYOUT = {
    "й": "q", "ц": "w", "у": "e", "к": "r", "е": "t", "н": "y", "г": "u", "ш": "i",
    "щ": "o", "з": "p", "х": "[", "ъ": "]", "ф": "a", "ы": "s", "в": "d", "а": "f",
    "п": "g", "р": "h", "о": "j", "л": "k", "д": "l", "ж": ";", "э": "'", "я": "z",
    "ч": "x", "с": "c", "м": "v", "и": "b", "т": "n", "ь": "m", "б": ",", "ю": ".",
}

CYR = re.compile(r"[\u0400-\u04FF]")
HOST_RX = re.compile(r"^(?=.{1,253}$)([a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)(\.[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)+$")
TLD_RX = re.compile(r"\.[a-z]{2,24}$")
DROP_HOSTS = {
    "google.com", "www.google.com", "google.ru", "www.google.ru",
    "yandex.ru", "yandex.com", "www.yandex.ru", "ya.ru",
    "bing.com", "www.bing.com", "duckduckgo.com", "search.yahoo.com",
}
TRACKING = re.compile(r"^(utm_|yclid|gclid|fbclid|msclkid|ref|referrer|from|source)$", re.I)


def _strip_invisible(s):
    s = unicodedata.normalize("NFKC", s)
    for ch in ("\u200b", "\u200c", "\u200d", "\ufeff", "\u00ad"):
        s = s.replace(ch, "")
    s = s.replace("\u00a0", " ").replace("\u202f", " ")
    s = s.replace("«", '"').replace("»", '"').replace("“", '"').replace("”", '"')
    s = s.replace("‘", "'").replace("’", "'").replace("–", "-").replace("—", "-")
    return s.strip()


def _from_redirect(s):
    """Extract the real target from archive/search redirects."""
    low = s.lower()
    if "web.archive.org/web/" in low:
        m = re.search(r"/web/[^/]+/(https?://.+)$", s, re.I)
        if m:
            return m.group(1)
        m = re.search(r"/web/[^/]+/([^/].*)$", s, re.I)
        if m:
            return "http://" + m.group(1)
    if any(h in low for h in ("google.", "yandex.", "bing.com", "duckduckgo.com")):
        try:
            q = urllib.parse.urlsplit(s).query
            params = urllib.parse.parse_qs(q)
            for key in ("q", "url", "u", "target", "text"):
                if key in params and params[key]:
                    cand = params[key][0]
                    if "://" in cand or "." in cand:
                        return cand
        except Exception:
            pass
        m = re.search(r"[?&](?:q|url|u)=([^&]+)", s)
        if m:
            return urllib.parse.unquote(m.group(1))
    return s


def _dekeyboard(s):
    """Convert text typed in RU layout to the Latin keys it represents."""
    out = []
    for ch in s:
        low = ch.lower()
        if low in RU_LAYOUT:
            mapped = RU_LAYOUT[low]
            out.append(mapped.upper() if ch.isupper() else mapped)
        else:
            out.append(ch)
    return "".join(out)


def _idna(host):
    try:
        return host.encode("idna").decode("ascii")
    except Exception:
        return None


def _candidate_tokens(s):
    s = s.replace("\r", "\n")
    raw = re.split(r"[\s,;]+", s)
    out = []
    for tok in raw:
        tok = tok.strip().strip('"\'`<>()[]{}.,;:!?')
        if not tok:
            continue
        if tok.lower().startswith(("http://", "https://", "//")):
            try:
                p = urllib.parse.urlsplit(tok if not tok.startswith("//") else "http:" + tok)
                if p.hostname:
                    out.append(p.hostname)
                    continue
            except Exception:
                pass
        if "." in tok and " " not in tok:
            out.append(tok.split("/")[0])
        elif CYR.search(tok) and "." in _dekeyboard(tok):
            out.append(_dekeyboard(tok))
    return out


def sanitize_site(text):
    """Return (host, note) or (None, error_key)."""
    if not text or not text.strip():
        return None, "empty"
    s = _strip_invisible(text)
    s = _from_redirect(s)

    # direct URL parse first
    try:
        if re.match(r"^(https?:)?//", s, re.I) and not re.search(r"\s", s):
            p = urllib.parse.urlsplit(s if not s.startswith("//") else "http:" + s)
            host = (p.hostname or "").strip(".")
            if host and "." in host:
                return _finish(host)
    except Exception:
        pass

    tokens = _candidate_tokens(s)
    if not tokens and "." in _dekeyboard(s):
        tokens = [_dekeyboard(s.split()[0])]
    if not tokens:
        return None, "no_host"

    host = tokens[0]
    return _finish(host)


def _finish(host):
    host = host.strip().strip(".").lower()
    if host.startswith("www."):
        host = host[4:]
    if not host:
        return None, "no_host"

    # 1) pure ASCII: validate as is
    if not CYR.search(host):
        if HOST_RX.match(host):
            return host, "ok"
        return None, "bad_host"

    # 2) Cyrillic present. Two cases: typed in RU layout, or a real IDN domain.
    dekeyed = _dekeyboard(host).lower().replace(",", ".")
    tld = ""
    if "." in host:
        tld = "." + host.rsplit(".", 1)[-1]
    latin_tld = bool(re.match(r"^\.[a-z]{2,24}$", tld))
    cyr_tld = bool(re.match(r"^\.[\u0400-\u04FF]{2,24}$", tld))

    if cyr_tld:
        puny = _idna(host)
        if puny and HOST_RX.match(puny):
            return puny, "idn"

    if latin_tld or HOST_RX.match(dekeyed):
        if HOST_RX.match(dekeyed):
            return dekeyed, "layout"

    puny = _idna(host)
    if puny and HOST_RX.match(puny):
        return puny, "idn"
    if HOST_RX.match(dekeyed):
        return dekeyed, "layout"
    return None, "bad_host"


def clean_path_query(url):
    """Drop tracking params and fragments from a full URL."""
    try:
        p = urllib.parse.urlsplit(url)
        q = [(k, v) for k, v in urllib.parse.parse_qsl(p.query, keep_blank_values=True)
             if not TRACKING.match(k)]
        return urllib.parse.urlunsplit((p.scheme, p.netloc, p.path, urllib.parse.urlencode(q), ""))
    except Exception:
        return url


if __name__ == "__main__":
    cases = [
        ("prokupol.ru", "prokupol.ru"),
        ("https://www.prokupol.ru/catalog/?utm_source=x#top", "prokupol.ru"),
        ("  «prokupol.ru»  ", "prokupol.ru"),
        ("Сайт: prokupol.ru — магазин", "prokupol.ru"),
        ("https://web.archive.org/web/20261016223707/https://prokupol.ru/", "prokupol.ru"),
        ("https://www.google.com/url?q=https%3A%2F%2Fprokupol.ru%2F&sa=D", "prokupol.ru"),
        ("зкщлгзщдюкг", "prokupol.ru"),
        ("PROKUPOL.RU", "prokupol.ru"),
        ("http://prokupol.ru:8080/x", "prokupol.ru"),
        ("prokupol.ru.", "prokupol.ru"),
        ("https://prokupol.ru\nhttps://other.ru", "prokupol.ru"),
        ("прокупол.рф", "xn--j1achbcce0a.xn--p1ai"),
        ("prokupol", None),
        ("", None),
    ]
    ok = 0
    for src, want in cases:
        got, note = sanitize_site(src)
        mark = "OK " if got == want else "FAIL"
        if got == want:
            ok += 1
        print("%s  %-55r -> %-32r (%s)" % (mark, src, got, note))
    print("%d/%d passed" % (ok, len(cases)))
