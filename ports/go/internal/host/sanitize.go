package host

import (
	"net/url"
	"strings"
	"unicode"
)

// Note — происхождение разобранного адреса (для журнала и отчёта).
type Note string

const (
	NotePlain   Note = "plain"
	NoteLayout  Note = "layout"
	NoteArchive Note = "archive"
	NoteSearch  Note = "search"
	NoteIDN     Note = "idn"
)

// ruLayout — соответствие «физическая клавиша» для ЙЦУКЕН -> QWERTY.
var ruLayout = map[rune]rune{
	'й': 'q', 'ц': 'w', 'у': 'e', 'к': 'r', 'е': 't', 'н': 'y', 'г': 'u', 'ш': 'i',
	'щ': 'o', 'з': 'p', 'х': '[', 'ъ': ']', 'ф': 'a', 'ы': 's', 'в': 'd', 'а': 'f',
	'п': 'g', 'р': 'h', 'о': 'j', 'л': 'k', 'д': 'l', 'ж': ';', 'э': '\'', 'я': 'z',
	'ч': 'x', 'с': 'c', 'м': 'v', 'и': 'b', 'т': 'n', 'ь': 'm', 'б': ',', 'ю': '.',
}

// cyrTLD — кириллические домены верхнего уровня: если метка из этого списка,
// ввод трактуется как IDN, а не как текст в русской раскладке.
var cyrTLD = map[string]bool{
	"рф": true, "укр": true, "срб": true, "бг": true, "ею": true, "москва": true,
	"онлайн": true, "сайт": true, "дети": true, "орг": true, "ком": true, "нет": true,
	"католик": true, "бел": true, "қаз": true, "мон": true,
}

// latinTLD — распространённые латинские зоны: нужны, чтобы отличить
// «prokupol.ru», набранное в русской раскладке, от кириллического IDN.
var latinTLD = map[string]bool{
	"ru": true, "com": true, "net": true, "org": true, "io": true, "su": true,
	"pro": true, "info": true, "biz": true, "me": true, "co": true, "ua": true,
	"by": true, "kz": true, "de": true, "uk": true, "eu": true, "us": true,
	"online": true, "site": true, "store": true, "shop": true, "tech": true,
	"dev": true, "app": true, "ai": true, "xyz": true, "top": true, "club": true,
	"cloud": true, "agency": true, "center": true, "company": true, "group": true,
	"media": true, "news": true, "life": true, "world": true, "space": true,
	"fun": true, "plus": true, "guru": true, "expert": true, "systems": true,
	"solutions": true, "services": true, "digital": true, "studio": true, "design": true,
	"moscow": true, "spb": true, "msk": true, "ru.net": true,
}

// Sanitize разбирает произвольный ввод и возвращает домен в ASCII (punycode для IDN).
// ok == false означает мусор: пустая строка, слово без точки, битый адрес.
func Sanitize(raw string) (string, Note, bool) {
	s := strings.TrimSpace(raw)
	if s == "" {
		return "", "", false
	}
	s = strings.Trim(s, " \t\r\n\"'`<>()[]{}«»“”„")
	if s == "" {
		return "", "", false
	}

	note := NotePlain
	origin := NotePlain
	for i := 0; i < 3; i++ {
		if tail, ok := fromArchive(s); ok {
			s = tail
			origin = NoteArchive
			continue
		}
		if tail, ok := fromSearchRedirect(s); ok {
			s = tail
			if origin == NotePlain {
				origin = NoteSearch
			}
			continue
		}
		break
	}
	for i := 0; i < 3; i++ {
		next := unescapeLoose(s)
		if next == s {
			break
		}
		s = next
	}

	if host, hn, ok := normalize(s); ok {
		note = pickNote(origin, hn)
		return host, note, true
	}
	// Текст вокруг адреса: «Сайт: prokupol.ru — магазин».
	for _, tok := range tokens(s) {
		if host, hn, ok := normalize(tok); ok {
			return host, pickNote(origin, hn), true
		}
	}
	return "", "", false
}

func pickNote(origin, hn Note) Note {
	if origin != NotePlain {
		return origin
	}
	return hn
}

// fromArchive вытаскивает адрес из ссылки веб-архива:
// https://web.archive.org/web/20261016223707/https://prokupol.ru/
// http://web.archive.org/web/20261016223707id_/http://prokupol.ru/
func fromArchive(s string) (string, bool) {
	low := strings.ToLower(s)
	idx := strings.Index(low, "/web/")
	if idx < 0 {
		return "", false
	}
	rest := s[idx+len("/web/"):]
	slash := strings.IndexByte(rest, '/')
	if slash <= 0 {
		return "", false
	}
	stamp := rest[:slash]
	if !strings.ContainsAny(stamp, "0123456789") {
		return "", false
	}
	tail := rest[slash+1:]
	if tail == "" {
		return "", false
	}
	if !strings.HasPrefix(strings.ToLower(tail), "http") {
		tail = "http://" + tail
	}
	return tail, true
}

// fromSearchRedirect вытаскивает адрес из редиректа поисковика:
// https://www.google.com/url?q=https%3A%2F%2Fprokupol.ru%2F
func fromSearchRedirect(s string) (string, bool) {
	u, err := url.Parse(s)
	if err != nil || u.Host == "" {
		return "", false
	}
	q := u.Query()
	hostLow := strings.ToLower(u.Host)
	engine := false
	for _, e := range []string{"google.", "yandex.", "bing.", "duckduckgo.", "mail.ru", "search."} {
		if strings.Contains(hostLow, e) {
			engine = true
			break
		}
	}
	for _, key := range []string{"q", "url", "u", "query", "target", "to"} {
		v := strings.TrimSpace(q.Get(key))
		if v == "" {
			continue
		}
		if strings.Contains(v, "://") || strings.HasPrefix(v, "//") {
			return v, true
		}
		if engine {
			if _, _, ok := normalize(v); ok {
				return v, true
			}
		}
	}
	return "", false
}

// unescapeLoose снимает процентное кодирование; двойное кодирование раскрывается
// за несколько проходов, но не больше трёх.
func unescapeLoose(s string) string {
	for i := 0; i < 3; i++ {
		dec, err := url.QueryUnescape(s)
		if err != nil || dec == s {
			break
		}
		s = dec
	}
	return s
}

// tokens режет текст вокруг адреса на кандидатов.
func tokens(s string) []string {
	fields := strings.FieldsFunc(s, func(r rune) bool {
		return unicode.IsSpace(r) || strings.ContainsRune(",;!?—–()[]{}<>«»\"'`", r)
	})
	out := make([]string, 0, len(fields))
	for _, f := range fields {
		f = strings.Trim(f, " \t.:;,-—–")
		if f != "" {
			out = append(out, f)
		}
	}
	return out
}

// normalize приводит один кандидат к домену.
func normalize(s string) (string, Note, bool) {
	h := strings.TrimSpace(s)
	if h == "" {
		return "", "", false
	}
	if i := strings.Index(h, "://"); i >= 0 {
		h = h[i+3:]
	} else {
		h = strings.TrimPrefix(h, "//")
	}
	if i := strings.IndexAny(h, "/?#"); i >= 0 {
		h = h[:i]
	}
	if i := strings.LastIndex(h, "@"); i >= 0 {
		h = h[i+1:]
	}
	if i := strings.LastIndex(h, ":"); i >= 0 && allDigits(h[i+1:]) {
		h = h[:i]
	}
	h = strings.TrimRight(h, ".")
	h = strings.ToLower(h)
	if h == "" || strings.ContainsAny(h, " \t") {
		return "", "", false
	}

	if hasCyrillic(h) {
		labels := strings.Split(h, ".")
		if len(labels) >= 2 && cyrTLD[labels[len(labels)-1]] {
			if p, err := ToASCII(h); err == nil && validHost(p) {
				return p, NoteIDN, true
			}
			return "", "", false
		}
		alt := strings.ToLower(dekeyboard(h))
		if alt != h && validHost(alt) && latinTLD[tldOf(alt)] {
			return alt, NoteLayout, true
		}
		if p, err := ToASCII(h); err == nil && validHost(p) {
			return p, NoteIDN, true
		}
		return "", "", false
	}
	if validHost(h) {
		return h, NotePlain, true
	}
	return "", "", false
}

func hasCyrillic(s string) bool {
	for _, r := range s {
		if unicode.Is(unicode.Cyrillic, r) {
			return true
		}
	}
	return false
}

func dekeyboard(s string) string {
	var b strings.Builder
	for _, r := range s {
		if q, ok := ruLayout[r]; ok {
			b.WriteRune(q)
			continue
		}
		if q, ok := ruLayout[unicode.ToLower(r)]; ok {
			b.WriteRune(q)
			continue
		}
		b.WriteRune(r)
	}
	return b.String()
}

func allDigits(s string) bool {
	if s == "" {
		return false
	}
	for i := 0; i < len(s); i++ {
		if s[i] < '0' || s[i] > '9' {
			return false
		}
	}
	return true
}

func tldOf(h string) string {
	if i := strings.LastIndex(h, "."); i >= 0 {
		return h[i+1:]
	}
	return ""
}

// validHost проверяет домен: 2+ метки, допустимые символы, осмысленная зона.
func validHost(h string) bool {
	if len(h) == 0 || len(h) > 253 {
		return false
	}
	labels := strings.Split(h, ".")
	if len(labels) < 2 {
		return false
	}
	for _, l := range labels {
		if len(l) == 0 || len(l) > 63 {
			return false
		}
		if l[0] == '-' || l[len(l)-1] == '-' {
			return false
		}
		for i := 0; i < len(l); i++ {
			c := l[i]
			if c >= 'a' && c <= 'z' || c >= '0' && c <= '9' || c == '-' {
				continue
			}
			return false
		}
	}
	tld := labels[len(labels)-1]
	if strings.HasPrefix(tld, "xn--") {
		return len(tld) > 4
	}
	if len(tld) < 2 {
		return false
	}
	for i := 0; i < len(tld); i++ {
		if tld[i] < 'a' || tld[i] > 'z' {
			return false
		}
	}
	return true
}
