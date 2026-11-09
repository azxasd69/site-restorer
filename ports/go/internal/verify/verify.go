// Package verify обходит готовый site/ и считает внутренние и битые ссылки.
package verify

import (
	"io/fs"
	"net/url"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"siterestorer/internal/assemble"
)

// Broken — одна битая ссылка.
type Broken struct {
	Page   string `json:"page"`
	Href   string `json:"href"`
	Target string `json:"target"`
}

// Stats — итог проверки.
type Stats struct {
	Pages    int      `json:"pages"`
	Internal int      `json:"internal"`
	Broken   int      `json:"broken"`
	External int      `json:"external"`
	Samples  []Broken `json:"samples"`
}

const maxSamples = 100

// Run проверяет все HTML-файлы в site и возвращает статистику.
func Run(site string) (Stats, error) {
	var st Stats
	files := map[string]bool{}
	err := filepath.WalkDir(site, func(p string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if d.IsDir() {
			return nil
		}
		rel, relErr := filepath.Rel(site, p)
		if relErr != nil {
			return relErr
		}
		files[filepath.ToSlash(rel)] = true
		return nil
	})
	if err != nil {
		return st, err
	}

	lower := map[string]bool{}
	for rel := range files {
		lower[strings.ToLower(rel)] = true
	}

	var pages []string
	for rel := range files {
		if isHTML(rel) {
			pages = append(pages, rel)
		}
	}
	sort.Strings(pages)
	st.Pages = len(pages)

	for _, page := range pages {
		data, err := os.ReadFile(filepath.Join(site, filepath.FromSlash(page)))
		if err != nil {
			continue
		}
		for _, ref := range ExtractLinks(string(data)) {
			v := strings.TrimSpace(ref)
			if v == "" || strings.HasPrefix(v, "#") {
				continue
			}
			low := strings.ToLower(v)
			if strings.HasPrefix(low, "mailto:") || strings.HasPrefix(low, "tel:") ||
				strings.HasPrefix(low, "javascript:") || strings.HasPrefix(low, "data:") {
				continue
			}
			if strings.Contains(v, "://") || strings.HasPrefix(v, "//") {
				u, err := url.Parse(v)
				if err != nil || u.Host != "" {
					st.External++
					continue
				}
			}
			st.Internal++
			target := resolve(page, v)
			if target == "" {
				continue
			}
			if !files[target] && !lower[strings.ToLower(target)] {
				st.Broken++
				if len(st.Samples) < maxSamples {
					st.Samples = append(st.Samples, Broken{Page: page, Href: v, Target: target})
				}
			}
		}
	}
	return st, nil
}

func isHTML(rel string) bool {
	ext := strings.ToLower(filepath.Ext(rel))
	switch ext {
	case ".html", ".htm", ".shtml", ".xhtml", ".php", ".asp", ".aspx", ".jsp", ".phtml":
		return true
	}
	return false
}

// resolve переводит ссылку в путь внутри site.
func resolve(page, ref string) string {
	if i := strings.IndexByte(ref, '#'); i >= 0 {
		ref = ref[:i]
	}
	if i := strings.IndexByte(ref, '?'); i >= 0 {
		ref = ref[:i]
	}
	if ref == "" {
		return page
	}
	if unescaped, err := url.PathUnescape(ref); err == nil {
		ref = unescaped
	}
	var rel string
	if strings.HasPrefix(ref, "/") {
		rel = strings.TrimPrefix(ref, "/")
	} else {
		rel = assemble.ResolveRel(page, ref)
	}
	if rel == "" {
		return "index.html"
	}
	if strings.HasSuffix(ref, "/") || !strings.Contains(pathBase(rel), ".") {
		rel = strings.TrimSuffix(rel, "/") + "/index.html"
		rel = strings.TrimPrefix(rel, "/")
	}
	return rel
}

func pathBase(p string) string {
	if i := strings.LastIndex(p, "/"); i >= 0 {
		return p[i+1:]
	}
	return p
}

// ExtractLinks собирает значения атрибутов-ссылок и url(...) из HTML.
// Поддерживаются и одинарные, и двойные кавычки, и значения без кавычек.
func ExtractLinks(doc string) []string {
	var out []string
	i := 0
	for i < len(doc) {
		j := strings.IndexByte(doc[i:], '<')
		if j < 0 {
			break
		}
		i += j
		if strings.HasPrefix(doc[i:], "<!--") {
			end := strings.Index(doc[i+4:], "-->")
			if end < 0 {
				break
			}
			i += 4 + end + 3
			continue
		}
		end := tagEnd(doc, i)
		if end < 0 {
			break
		}
		tag := doc[i : end+1]
		i = end + 1
		attrs := parseAttrs(tag)
		for name, val := range attrs {
			low := strings.ToLower(name)
			switch {
			case low == "href" || low == "src" || low == "data-src" || low == "data-bg" ||
				low == "poster" || low == "action" || low == "data-original":
				out = append(out, val)
			case low == "srcset" || low == "data-srcset":
				for _, part := range strings.Split(val, ",") {
					fields := strings.Fields(strings.TrimSpace(part))
					if len(fields) > 0 {
						out = append(out, fields[0])
					}
				}
			case low == "style":
				out = append(out, cssURLs(val)...)
			}
		}
	}
	return out
}

func parseAttrs(tag string) map[string]string {
	attrs := map[string]string{}
	i := 1
	for i < len(tag) && !isSpace(tag[i]) && tag[i] != '>' && tag[i] != '/' {
		i++
	}
	for i < len(tag) {
		for i < len(tag) && isSpace(tag[i]) {
			i++
		}
		if i >= len(tag) || tag[i] == '>' || tag[i] == '/' {
			break
		}
		start := i
		for i < len(tag) && !isSpace(tag[i]) && tag[i] != '=' && tag[i] != '>' && tag[i] != '/' {
			i++
		}
		name := tag[start:i]
		for i < len(tag) && isSpace(tag[i]) {
			i++
		}
		if i >= len(tag) || tag[i] != '=' {
			attrs[strings.ToLower(name)] = ""
			continue
		}
		i++
		for i < len(tag) && isSpace(tag[i]) {
			i++
		}
		if i < len(tag) && (tag[i] == '"' || tag[i] == '\'') {
			q := tag[i]
			i++
			vs := i
			for i < len(tag) && tag[i] != q {
				i++
			}
			attrs[strings.ToLower(name)] = tag[vs:i]
			if i < len(tag) {
				i++
			}
			continue
		}
		vs := i
		for i < len(tag) && !isSpace(tag[i]) && tag[i] != '>' {
			i++
		}
		attrs[strings.ToLower(name)] = tag[vs:i]
	}
	return attrs
}

func cssURLs(css string) []string {
	var out []string
	low := strings.ToLower(css)
	i := 0
	for {
		j := strings.Index(low[i:], "url(")
		if j < 0 {
			break
		}
		j += i + len("url(")
		for j < len(css) && (css[j] == ' ' || css[j] == '\t') {
			j++
		}
		if j < len(css) && (css[j] == '"' || css[j] == '\'') {
			q := css[j]
			j++
			start := j
			for j < len(css) && css[j] != q {
				j++
			}
			out = append(out, css[start:j])
			i = j
			continue
		}
		start := j
		for j < len(css) && css[j] != ')' && css[j] != ' ' {
			j++
		}
		out = append(out, css[start:j])
		i = j
	}
	return out
}

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

func isSpace(c byte) bool {
	return c == ' ' || c == '\t' || c == '\n' || c == '\r'
}
