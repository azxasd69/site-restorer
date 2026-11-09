// Package localpath превращает URL в детерминированный относительный путь файла.
package localpath

import (
	"fmt"
	"net/url"
	"path"
	"strings"
)

// MaxLen — предельная длина относительного пути (в байтах).
const MaxLen = 160

// MaxSegment — предел длины одного сегмента (имена файлов в Windows/FAT).
const MaxSegment = 63

var reservedNames = map[string]bool{
	"con": true, "prn": true, "aux": true, "nul": true,
	"com1": true, "com2": true, "com3": true, "com4": true, "com5": true,
	"com6": true, "com7": true, "com8": true, "com9": true,
	"lpt1": true, "lpt2": true, "lpt3": true, "lpt4": true, "lpt5": true,
	"lpt6": true, "lpt7": true, "lpt8": true, "lpt9": true,
}

// ForURL возвращает относительный путь (со слешами) для абсолютного URL.
// Один и тот же URL всегда даёт один и тот же путь.
func ForURL(raw string) (string, error) {
	u, err := parse(raw)
	if err != nil {
		return "", err
	}
	if u.Host == "" {
		return "", fmt.Errorf("в адресе нет хоста: %q", raw)
	}

	escaped := u.EscapedPath()
	trailing := escaped == "" || strings.HasSuffix(escaped, "/")

	var segs []string
	for _, part := range strings.Split(escaped, "/") {
		if part == "" {
			continue
		}
		if dec, err := url.PathUnescape(part); err == nil {
			part = dec
		}
		segs = append(segs, cleanSegment(part))
	}
	if len(segs) == 0 {
		trailing = true
	}

	if trailing {
		segs = append(segs, "index.html")
	} else if last := segs[len(segs)-1]; !strings.Contains(last, ".") {
		// /catalog/about -> catalog/about/index.html
		segs = append(segs, "index.html")
	}

	if u.RawQuery != "" {
		segs[len(segs)-1] = withQueryHash(segs[len(segs)-1], u.RawQuery)
	}

	return fit(strings.Join(segs, "/")), nil
}

// ForDirURL — путь каталога (для ссылок на «/path/» без файла).
func ForDirURL(raw string) (string, error) {
	return ForURL(raw)
}

func parse(raw string) (*url.URL, error) {
	s := strings.TrimSpace(raw)
	if s == "" {
		return nil, fmt.Errorf("пустой адрес")
	}
	if !strings.Contains(s, "://") && !strings.HasPrefix(s, "//") {
		s = "http://" + s
	}
	u, err := url.Parse(s)
	if err != nil {
		return nil, err
	}
	if u.Scheme != "http" && u.Scheme != "https" {
		return nil, fmt.Errorf("неподдерживаемая схема: %q", u.Scheme)
	}
	return u, nil
}

// cleanSegment обезвреживает один сегмент пути.
func cleanSegment(s string) string {
	s = strings.TrimSpace(s)
	if s == "" || s == "." || s == ".." {
		return "_"
	}
	var b strings.Builder
	for _, r := range s {
		switch {
		case r < 0x20 || r == 0x7f:
			// управляющие символы выбрасываем
		case r == ' ' || r == '/' || r == '<' || r == '>' || r == ':' || r == '"' ||
			r == '\\' || r == '|' || r == '?' || r == '*' || r == '~':
			// запрещённые Windows символы и наш собственный escape-префикс ~
			for _, by := range []byte(string(r)) {
				fmt.Fprintf(&b, "~%02X", by)
			}
		case r >= 0x80:
			// не-ASCII пишем байтами UTF-8 через ~XX: имя остаётся ASCII и переносимым
			for _, by := range []byte(string(r)) {
				fmt.Fprintf(&b, "~%02X", by)
			}
		default:
			b.WriteRune(r)
		}
	}
	out := strings.TrimRight(b.String(), ". ")
	if out == "" {
		out = "_"
	}
	if len(out) > MaxSegment {
		ext := path.Ext(out)
		stem := strings.TrimSuffix(out, ext)
		out = stem[:MaxSegment-len(ext)-8] + "~" + hash6(stem) + ext
	}
	if reservedNames[strings.ToLower(stemOf(out))] {
		out = "_" + out
	}
	return out
}

func stemOf(name string) string {
	if i := strings.IndexByte(name, '.'); i >= 0 {
		return name[:i]
	}
	return name
}

// withQueryHash вставляет хэш query-строки перед расширением.
func withQueryHash(name, query string) string {
	h := hash8(query)
	ext := path.Ext(name)
	stem := strings.TrimSuffix(name, ext)
	if stem == "" {
		stem = "index"
	}
	return stem + "-q" + h + ext
}

// fit укладывает путь в MaxLen, сохраняя детерминированность.
func fit(rel string) string {
	if len(rel) <= MaxLen {
		return rel
	}
	segs := strings.Split(rel, "/")
	for i := 0; i < len(segs)-1; i++ {
		if len(segs[i]) > 20 {
			segs[i] = segs[i][:12] + "~" + hash6(segs[i])
		}
	}
	rel = strings.Join(segs, "/")
	if len(rel) <= MaxLen {
		return rel
	}
	ext := path.Ext(segs[len(segs)-1])
	return "_long/" + hash8(rel) + ext
}

// hash32 — FNV-1a, 32 бита.
func hash32(s string) uint32 {
	var h uint32 = 2166136261
	for i := 0; i < len(s); i++ {
		h ^= uint32(s[i])
		h *= 16777619
	}
	return h
}

func hash8(s string) string { return fmt.Sprintf("%08x", hash32(s)) }
func hash6(s string) string { return fmt.Sprintf("%08x", hash32(s))[:6] }

// CleanRel нормализует относительный путь и не выпускает его выше корня:
// «..» на верхнем уровне просто отбрасывается.
func CleanRel(p string) string {
	parts := strings.Split(strings.ReplaceAll(p, "\\", "/"), "/")
	out := make([]string, 0, len(parts))
	for _, seg := range parts {
		switch seg {
		case "", ".":
			continue
		case "..":
			if len(out) > 0 {
				out = out[:len(out)-1]
			}
		default:
			out = append(out, seg)
		}
	}
	return strings.Join(out, "/")
}
