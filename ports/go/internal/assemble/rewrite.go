// Package assemble собирает копию сайта: переписывает ссылки в относительные,
// приводит кодировки к UTF-8 и раскладывает файлы в дерево site/.
package assemble

import (
	"net/url"
	"strings"

	"siterestorer/internal/localpath"
)

// LinkStats — счётчики переписывания.
type LinkStats struct {
	Links    int // внутренних ссылок встречено
	Rewrote  int // переписано в относительные
	Missing  int // цель отсутствует в копии
	External int // внешних ссылок (не трогали)
}

var linkAttrs = map[string]bool{
	"href": true, "src": true, "data-src": true, "data-bg": true,
	"poster": true, "action": true, "data-original": true,
}

var srcsetAttrs = map[string]bool{"srcset": true, "data-srcset": true}

// Rewriter переписывает ссылки одного документа.
type Rewriter struct {
	L    *Linker
	Base *url.URL
	Cur  string
	Stat *LinkStats
}

// HTML переписывает документ: атрибуты, srcset, inline-стили, <base> удаляется.
func (rw *Rewriter) HTML(doc string) string {
	var b strings.Builder
	b.Grow(len(doc))
	i := 0
	for i < len(doc) {
		j := strings.IndexByte(doc[i:], '<')
		if j < 0 {
			b.WriteString(doc[i:])
			break
		}
		b.WriteString(doc[i : i+j])
		i += j

		switch {
		case strings.HasPrefix(doc[i:], "<!--"):
			end := strings.Index(doc[i+4:], "-->")
			if end < 0 {
				b.WriteString(doc[i:])
				return b.String()
			}
			b.WriteString(doc[i : i+4+end+3])
			i += 4 + end + 3
			continue
		case strings.HasPrefix(doc[i:], "<!") || strings.HasPrefix(doc[i:], "<?"):
			end := strings.IndexByte(doc[i:], '>')
			if end < 0 {
				b.WriteString(doc[i:])
				return b.String()
			}
			b.WriteString(doc[i : i+end+1])
			i += end + 1
			continue
		}

		end := tagEnd(doc, i)
		if end < 0 {
			b.WriteString(doc[i:])
			break
		}
		tag := doc[i : end+1]
		i = end + 1

		name := tagName(tag)
		if name == "base" {
			continue // <base> ломает относительные ссылки — убираем
		}
		b.WriteString(rw.tag(tag))

		// Содержимое style переписываем как CSS, содержимое script не трогаем.
		if name == "style" {
			if closeIdx := indexFold(doc[i:], "</style"); closeIdx >= 0 {
				b.WriteString(rw.CSS(doc[i : i+closeIdx]))
				i += closeIdx
			}
		} else if name == "script" {
			if closeIdx := indexFold(doc[i:], "</script"); closeIdx >= 0 {
				b.WriteString(doc[i : i+closeIdx])
				i += closeIdx
			}
		}
	}
	return b.String()
}

// tag переписывает атрибуты внутри одного тега.
func (rw *Rewriter) tag(tag string) string {
	return rewriteAttrs(tag, func(name, val, quote string) (string, bool) {
		low := strings.ToLower(name)
		if linkAttrs[low] {
			if nv, ok := rw.URL(val); ok {
				return nv, true
			}
			return "", false
		}
		if srcsetAttrs[low] {
			if nv, ok := rw.srcset(val); ok {
				return nv, true
			}
			return "", false
		}
		if low == "style" {
			nv := rw.CSS(val)
			if nv != val {
				return nv, true
			}
		}
		return "", false
	})
}

// URL переписывает одну ссылку. Возвращает новое значение и признак изменения.
func (rw *Rewriter) URL(raw string) (string, bool) {
	value := strings.TrimSpace(raw)
	if value == "" {
		return "", false
	}
	frag := ""
	if idx := strings.IndexByte(value, '#'); idx >= 0 {
		frag = value[idx:]
		value = value[:idx]
	}
	if strings.HasPrefix(value, "#") || value == "" {
		return "", false
	}

	target, internal, exists := rw.L.Resolve(value, rw.Base)
	if !internal {
		rw.Stat.External++
		return "", false
	}
	rw.Stat.Links++
	if !exists {
		rw.Stat.Missing++
	}
	rw.Stat.Rewrote++
	return HrefPercent(RelLink(rw.Cur, target)) + frag, true
}

// srcset переписывает список кандидатов «url descriptor, url descriptor».
func (rw *Rewriter) srcset(val string) (string, bool) {
	parts := strings.Split(val, ",")
	changed := false
	out := make([]string, 0, len(parts))
	for _, part := range parts {
		item := strings.TrimSpace(part)
		if item == "" {
			continue
		}
		fields := strings.Fields(item)
		if len(fields) == 0 {
			continue
		}
		u := fields[0]
		rest := strings.TrimSpace(strings.TrimPrefix(item, u))
		if nv, ok := rw.URL(u); ok {
			fields[0] = nv
			changed = true
		}
		res := fields[0]
		if rest != "" {
			res += " " + rest
		}
		out = append(out, res)
	}
	if !changed {
		return "", false
	}
	return strings.Join(out, ", "), true
}

// CSS переписывает url(...) и @import в таблице стилей или inline-стиле.
func (rw *Rewriter) CSS(css string) string {
	var b strings.Builder
	lower := strings.ToLower(css)
	i := 0
	for i < len(css) {
		j := strings.Index(lower[i:], "url(")
		if j < 0 {
			b.WriteString(css[i:])
			break
		}
		j += i
		b.WriteString(css[i:j])
		k := j + len("url(")
		for k < len(css) && (css[k] == ' ' || css[k] == '\t' || css[k] == '\n' || css[k] == '\r') {
			k++
		}
		var quote byte
		if k < len(css) && (css[k] == '"' || css[k] == '\'') {
			quote = css[k]
			k++
		}
		start := k
		for k < len(css) {
			c := css[k]
			if quote != 0 {
				if c == quote {
					break
				}
			} else if c == ')' || c == ' ' || c == '\n' || c == '\t' {
				break
			}
			k++
		}
		rawURL := css[start:k]
		if nv, ok := rw.URL(rawURL); ok {
			b.WriteString("url(")
			if quote != 0 {
				b.WriteByte(quote)
			}
			b.WriteString(nv)
			if quote != 0 {
				b.WriteByte(quote)
				if k < len(css) && css[k] == quote {
					k++
				}
			}
			b.WriteString(")")
			// пропускаем исходные кавычку и закрывающую скобку
			for k < len(css) && css[k] != ')' {
				k++
			}
			if k < len(css) {
				k++
			}
			i = k
			continue
		}
		b.WriteString(css[j:k])
		i = k
	}
	return b.String()
}

// tagEnd находит закрывающую '>' тега, начиная с позиции '<'.
func tagEnd(s string, start int) int {
	i := start + 1
	for i < len(s) {
		c := s[i]
		if c == '\'' || c == '"' {
			q := c
			i++
			for i < len(s) && s[i] != q {
				i++
			}
			if i < len(s) {
				i++
			}
			continue
		}
		if c == '>' {
			return i
		}
		i++
	}
	return -1
}

// tagName вытаскивает имя тега из его текста.
func tagName(tag string) string {
	i := 1
	for i < len(tag) && (tag[i] == '/' || tag[i] == '!' || tag[i] == '?') {
		i++
	}
	start := i
	for i < len(tag) {
		c := tag[i]
		if c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '>' || c == '/' {
			break
		}
		i++
	}
	return strings.ToLower(tag[start:i])
}

// rewriteAttrs разбирает тег и подменяет значения атрибутов.
func rewriteAttrs(tag string, fn func(name, val, quote string) (string, bool)) string {
	var b strings.Builder
	b.Grow(len(tag))
	i := 1
	for i < len(tag) && !isSpaceByte(tag[i]) && tag[i] != '>' && tag[i] != '/' {
		i++
	}
	b.WriteString(tag[:i])

	for i < len(tag) {
		ws := i
		for i < len(tag) && isSpaceByte(tag[i]) {
			i++
		}
		b.WriteString(tag[ws:i])
		if i >= len(tag) {
			break
		}
		if tag[i] == '>' {
			b.WriteString(tag[i:])
			break
		}
		if tag[i] == '/' {
			b.WriteString(tag[i:])
			break
		}

		nameStart := i
		for i < len(tag) && !isSpaceByte(tag[i]) && tag[i] != '=' && tag[i] != '>' && tag[i] != '/' {
			i++
		}
		name := tag[nameStart:i]
		b.WriteString(name)

		if i >= len(tag) || tag[i] != '=' {
			continue
		}
		i++ // '='
		b.WriteByte('=')

		if i < len(tag) && (tag[i] == '"' || tag[i] == '\'') {
			q := tag[i]
			i++
			vs := i
			for i < len(tag) && tag[i] != q {
				i++
			}
			val := tag[vs:i]
			if i < len(tag) {
				i++
			}
			if nv, ok := fn(name, val, string(q)); ok {
				b.WriteByte(q)
				b.WriteString(nv)
				b.WriteByte(q)
			} else {
				b.WriteByte(q)
				b.WriteString(val)
				b.WriteByte(q)
			}
			continue
		}

		vs := i
		for i < len(tag) && !isSpaceByte(tag[i]) && tag[i] != '>' {
			i++
		}
		val := tag[vs:i]
		if nv, ok := fn(name, val, ""); ok {
			if needsQuote(nv) {
				b.WriteByte('"')
				b.WriteString(nv)
				b.WriteByte('"')
			} else {
				b.WriteString(nv)
			}
		} else {
			b.WriteString(val)
		}
	}
	return b.String()
}

func needsQuote(s string) bool {
	for i := 0; i < len(s); i++ {
		switch s[i] {
		case ' ', '\t', '\n', '"', '\'', '>', '=', '`':
			return true
		}
	}
	return false
}

func indexFold(s, sub string) int {
	return strings.Index(strings.ToLower(s), strings.ToLower(sub))
}

// StripBase убирает теги <base ...> (используется и отдельно, для проверки).
func StripBase(doc string) string {
	var b strings.Builder
	i := 0
	for {
		j := indexFold(doc[i:], "<base")
		if j < 0 {
			b.WriteString(doc[i:])
			break
		}
		j += i
		b.WriteString(doc[i:j])
		end := tagEnd(doc, j)
		if end < 0 {
			b.WriteString(doc[j:])
			break
		}
		i = end + 1
	}
	return b.String()
}

// ResolveRel разрешает относительную ссылку из файла cur (для проверки).
func ResolveRel(cur, ref string) string {
	if ref == "" {
		return ""
	}
	if strings.HasPrefix(ref, "/") {
		ref = strings.TrimPrefix(ref, "/")
	}
	base := pathDir(cur)
	joined := ref
	if base != "" && !strings.HasPrefix(ref, "/") {
		joined = base + "/" + ref
	}
	return cleanPath(joined)
}

func pathDir(p string) string {
	i := strings.LastIndex(p, "/")
	if i < 0 {
		return ""
	}
	return p[:i]
}

func cleanPath(p string) string {
	return localpath.CleanRel(p)
}
