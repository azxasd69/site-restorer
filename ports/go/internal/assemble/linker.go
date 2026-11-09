package assemble

import (
	"net/url"
	"path"
	"sort"
	"strconv"
	"strings"

	"siterestorer/internal/localpath"
)

// Linker знает, какие файлы есть в копии, и умеет переводить ссылки в локальные пути.
type Linker struct {
	Primary string
	Files   map[string]bool
	origins map[string]string
	ext     map[string]bool
	byBase  map[string][]string
	byDepth map[string][]string
}

// NewLinker создаёт пустой справочник для основного домена.
func NewLinker(primary string) *Linker {
	return &Linker{
		Primary: CanonicalHost(primary),
		Files:   map[string]bool{},
		origins: map[string]string{},
		ext:     map[string]bool{},
		byBase:  map[string][]string{},
		byDepth: map[string][]string{},
	}
}

// Add регистрирует файл копии и его исходный URL.
func (l *Linker) Add(rel, original string) {
	rel = strings.TrimPrefix(rel, "/")
	l.Files[rel] = true
	if original != "" {
		l.origins[rel] = original
	}
	if host := hostOf(original); host != "" && !SameSite(l.Primary, host) {
		l.ext[CanonicalHost(host)] = true
	}
}

// Original возвращает исходный URL файла копии.
func (l *Linker) Original(rel string) string { return l.origins[rel] }

// Index строит поисковые индексы по имени и по глубине.
func (l *Linker) Index() {
	for rel := range l.Files {
		base := strings.ToLower(path.Base(rel))
		l.byBase[base] = append(l.byBase[base], rel)
		k := depthKey(rel)
		l.byDepth[k] = append(l.byDepth[k], rel)
	}
	for k := range l.byBase {
		sort.Strings(l.byBase[k])
	}
	for k := range l.byDepth {
		sort.Strings(l.byDepth[k])
	}
}

func depthKey(rel string) string {
	return strconv.Itoa(strings.Count(rel, "/")) + "|" + strings.ToLower(path.Base(rel))
}

func depthOf(rel string) int { return strings.Count(rel, "/") }

func hostOf(raw string) string {
	if raw == "" {
		return ""
	}
	u, err := url.Parse(raw)
	if err != nil {
		return ""
	}
	return u.Host
}

// Resolve переводит ссылку в путь внутри копии.
// internal — ссылка ведёт на свой домен (или на скачанный внешний хост),
// exists — файл реально есть в копии.
func (l *Linker) Resolve(ref string, base *url.URL) (target string, internal, exists bool) {
	ref = strings.TrimSpace(ref)
	if ref == "" || strings.HasPrefix(ref, "#") {
		return "", false, false
	}
	low := strings.ToLower(ref)
	for _, p := range []string{"mailto:", "tel:", "javascript:", "data:", "blob:", "about:", "ftp:", "whatsapp:", "viber:", "skype:"} {
		if strings.HasPrefix(low, p) {
			return "", false, false
		}
	}

	ru, err := url.Parse(ref)
	if err != nil {
		return "", false, false
	}
	var abs *url.URL
	if ru.IsAbs() {
		abs = ru
	} else {
		if base == nil {
			return "", false, false
		}
		abs = base.ResolveReference(ru)
	}
	if abs.Host == "" || (abs.Scheme != "http" && abs.Scheme != "https") {
		return "", false, false
	}
	host := CanonicalHost(abs.Host)

	rel, err := LocalRel(l.Primary, abs.String())
	if err != nil {
		return "", false, false
	}
	if !SameSite(l.Primary, host) && !l.ext[host] {
		return "", false, false
	}
	if l.Files[rel] {
		return rel, true, true
	}
	if alt, ok := l.heuristics(rel); ok {
		return alt, true, true
	}
	return rel, true, false
}

func hostDir(host string) string {
	var b strings.Builder
	for _, r := range host {
		switch {
		case r >= 'a' && r <= 'z', r >= '0' && r <= '9', r == '.', r == '-':
			b.WriteRune(r)
		default:
			b.WriteByte('_')
		}
	}
	return strings.Trim(b.String(), ".")
}

// CanonicalHost приводит хост к нижнему регистру, снимает порт по умолчанию
// и отбрасывает «www.» — так www.example.com и example.com считаются одним сайтом.
func CanonicalHost(h string) string {
	h = strings.ToLower(strings.TrimSpace(h))
	h = strings.TrimSuffix(h, ":80")
	h = strings.TrimSuffix(h, ":443")
	h = strings.TrimSuffix(h, ".")
	return strings.TrimPrefix(h, "www.")
}

// SameSite отвечает, относится ли хост к тому же сайту, что и основной.
func SameSite(primary, host string) bool {
	return CanonicalHost(primary) == CanonicalHost(host)
}

// LocalRel возвращает путь файла в копии с учётом внешних хостов: файлы чужих
// доменов живут в _ext/<хост>/...
func LocalRel(primary, rawURL string) (string, error) {
	rel, err := localpath.ForURL(rawURL)
	if err != nil {
		return "", err
	}
	u, err := url.Parse(rawURL)
	if err != nil {
		return "", err
	}
	host := CanonicalHost(u.Host)
	if host == "" || SameSite(primary, host) {
		return rel, nil
	}
	return "_ext/" + hostDir(host) + "/" + rel, nil
}

// heuristics ищет замену отсутствующему файлу по типовым правилам CMS.
func (l *Linker) heuristics(rel string) (string, bool) {
	// 1. upload/resize_cache/iblock/<aa>/<size>/<name> -> upload/iblock/<aa>/<name>
	if i := strings.Index(rel, "upload/resize_cache/iblock/"); i >= 0 {
		prefix := rel[:i]
		rest := rel[i+len("upload/resize_cache/iblock/"):]
		parts := strings.Split(rest, "/")
		if len(parts) >= 3 {
			name := strings.Join(parts[2:], "/")
			cand := prefix + "upload/iblock/" + parts[0] + "/" + name
			if l.Files[cand] {
				return cand, true
			}
			if alt := l.byBaseUnder(strings.ToLower(path.Base(cand)), prefix+"upload/iblock/"); alt != "" {
				return alt, true
			}
		}
	}

	// 2. bitrix/cache/(css|js)/s1/.../<name>_<hash>_v1.ext -> файл того же типа
	if strings.Contains(rel, "bitrix/cache/css/") || strings.Contains(rel, "bitrix/cache/js/") {
		if alt := l.cacheLike(rel); alt != "" {
			return alt, true
		}
	}

	// 3. то же имя на той же глубине
	if cands := l.byDepth[depthKey(rel)]; len(cands) > 0 {
		return cands[0], true
	}

	// 4. то же имя где угодно
	if cands := l.byBase[strings.ToLower(path.Base(rel))]; len(cands) > 0 {
		return cands[0], true
	}
	return "", false
}

// byBaseUnder ищет файл по имени внутри поддерева.
func (l *Linker) byBaseUnder(base, prefix string) string {
	for _, cand := range l.byBase[base] {
		if strings.HasPrefix(cand, prefix) {
			return cand
		}
	}
	return ""
}

// cacheLike сопоставляет кэшированный файл Bitrix любому файлу того же типа
// с тем же префиксом имени (хэш в имени меняется при регенерации кэша).
func (l *Linker) cacheLike(rel string) string {
	kind := "bitrix/cache/css/"
	if strings.Contains(rel, "bitrix/cache/js/") {
		kind = "bitrix/cache/js/"
	}
	prefix, ext, ok := splitCacheName(path.Base(rel))
	if !ok {
		return ""
	}
	best := ""
	for cand := range l.Files {
		if !strings.Contains(cand, kind) {
			continue
		}
		if path.Ext(cand) != ext {
			continue
		}
		if !strings.HasPrefix(path.Base(cand), prefix+"_") {
			continue
		}
		if best == "" || cand < best {
			best = cand
		}
	}
	return best
}

// splitCacheName разбирает <name>_<hash>_v1.ext.
func splitCacheName(base string) (prefix, ext string, ok bool) {
	ext = path.Ext(base)
	stem := strings.TrimSuffix(base, ext)
	if !strings.HasSuffix(stem, "_v1") {
		// допускаем _v2, _v3 ...
		i := strings.LastIndex(stem, "_v")
		if i < 0 {
			return "", "", false
		}
		if _, err := strconv.Atoi(stem[i+2:]); err != nil {
			return "", "", false
		}
		stem = stem[:i]
	} else {
		stem = strings.TrimSuffix(stem, "_v1")
	}
	i := strings.LastIndex(stem, "_")
	if i <= 0 {
		return "", "", false
	}
	prefix = stem[:i]
	if prefix == "" {
		return "", "", false
	}
	return prefix, ext, true
}

// RelLink считает относительную ссылку из файла from в файл to.
func RelLink(from, to string) string {
	fromDir := path.Dir(strings.TrimPrefix(from, "/"))
	if fromDir == "." {
		fromDir = ""
	}
	fromParts := splitNonEmpty(fromDir)
	toParts := splitNonEmpty(strings.TrimPrefix(to, "/"))
	if len(toParts) == 0 {
		return ""
	}
	i := 0
	for i < len(fromParts) && i < len(toParts)-1 && fromParts[i] == toParts[i] {
		i++
	}
	var b strings.Builder
	for k := i; k < len(fromParts); k++ {
		b.WriteString("../")
	}
	b.WriteString(strings.Join(toParts[i:], "/"))
	out := b.String()
	if out == "" {
		out = path.Base(to)
	}
	return out
}

func splitNonEmpty(p string) []string {
	if p == "" || p == "." {
		return nil
	}
	raw := strings.Split(p, "/")
	out := make([]string, 0, len(raw))
	for _, s := range raw {
		if s != "" && s != "." {
			out = append(out, s)
		}
	}
	return out
}

// HrefPercent кодирует локальный путь для href: пробелы и не-ASCII -> %XX.
func HrefPercent(rel string) string {
	var b strings.Builder
	for _, seg := range strings.Split(rel, "/") {
		if b.Len() > 0 || strings.HasPrefix(rel, "/") {
			b.WriteByte('/')
		}
		for i := 0; i < len(seg); i++ {
			c := seg[i]
			if c >= 0x80 || c == ' ' || c == '#' || c == '?' {
				b.WriteString("%")
				b.WriteString(hexUpper(c))
				continue
			}
			b.WriteByte(c)
		}
	}
	return b.String()
}

const hexDigits = "0123456789ABCDEF"

func hexUpper(c byte) string {
	return string([]byte{hexDigits[c>>4], hexDigits[c&0x0f]})
}
