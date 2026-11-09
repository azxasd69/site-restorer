package assemble

import (
	"fmt"
	"io/fs"
	"net/url"
	"os"
	"path/filepath"
	"strings"
)

// KindStat — счётчик по типу файлов.
type KindStat struct {
	Count int   `json:"count"`
	Bytes int64 `json:"bytes"`
}

// Stats — итоги сборки.
type Stats struct {
	Files    int      `json:"files"`
	Bytes    int64    `json:"bytes"`
	HTML     KindStat `json:"html"`
	CSS      KindStat `json:"css"`
	Code     KindStat `json:"code"`
	Img      KindStat `json:"img"`
	Other    KindStat `json:"other"`
	Links    int      `json:"links"`
	Rewrote  int      `json:"rewrote"`
	Missing  int      `json:"missing"`
	External int      `json:"external"`
}

// Options — параметры сборки.
type Options struct {
	Mirror  string
	Site    string
	Primary string
	Origins map[string]string // rel -> исходный URL
	Logf    func(format string, args ...any)
}

var (
	htmlExt = map[string]bool{".html": true, ".htm": true, ".shtml": true, ".xhtml": true,
		".php": true, ".asp": true, ".aspx": true, ".jsp": true, ".cgi": true, ".phtml": true}
	cssExt  = map[string]bool{".css": true}
	codeExt = map[string]bool{".js": true, ".mjs": true, ".json": true, ".xml": true,
		".map": true, ".woff": true, ".woff2": true, ".ttf": true, ".eot": true}
	imgExt = map[string]bool{".png": true, ".jpg": true, ".jpeg": true, ".gif": true,
		".webp": true, ".svg": true, ".ico": true, ".bmp": true, ".avif": true, ".tif": true, ".tiff": true}
)

// Run собирает site/ из mirror/ и возвращает статистику.
func Run(opts Options) (Stats, error) {
	var st Stats
	if opts.Logf == nil {
		opts.Logf = func(string, ...any) {}
	}
	if err := os.MkdirAll(opts.Site, 0o755); err != nil {
		return st, err
	}

	l := NewLinker(opts.Primary)
	var files []string
	err := filepath.WalkDir(opts.Mirror, func(p string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if d.IsDir() {
			return nil
		}
		rel, relErr := filepath.Rel(opts.Mirror, p)
		if relErr != nil {
			return relErr
		}
		rel = filepath.ToSlash(rel)
		files = append(files, rel)
		l.Add(rel, opts.Origins[rel])
		return nil
	})
	if err != nil {
		return st, fmt.Errorf("обход mirror: %w", err)
	}
	l.Index()
	opts.Logf("сборка: файлов в зеркале — %d", len(files))

	for _, rel := range files {
		src := filepath.Join(opts.Mirror, filepath.FromSlash(rel))
		data, err := os.ReadFile(src)
		if err != nil {
			return st, fmt.Errorf("чтение %s: %w", rel, err)
		}
		dst := filepath.Join(opts.Site, filepath.FromSlash(rel))
		if err := os.MkdirAll(filepath.Dir(dst), 0o755); err != nil {
			return st, err
		}

		kind := kindOf(rel, data)
		base := baseURL(opts.Origins[rel], opts.Primary, rel)

		switch kind {
		case "html":
			doc := DecodeToUTF8(data, false)
			rw := &Rewriter{L: l, Base: base, Cur: rel, Stat: &LinkStats{}}
			out := rw.HTML(doc)
			out = normalizeCharset(out, false)
			st.Links += rw.Stat.Links
			st.Rewrote += rw.Stat.Rewrote
			st.Missing += rw.Stat.Missing
			st.External += rw.Stat.External
			data = []byte(out)
			st.HTML.Count++
			st.HTML.Bytes += int64(len(data))
		case "css":
			doc := DecodeToUTF8(data, true)
			rw := &Rewriter{L: l, Base: base, Cur: rel, Stat: &LinkStats{}}
			out := rw.CSS(doc)
			out = normalizeCharset(out, true)
			st.Links += rw.Stat.Links
			st.Rewrote += rw.Stat.Rewrote
			st.Missing += rw.Stat.Missing
			st.External += rw.Stat.External
			data = []byte(out)
			st.CSS.Count++
			st.CSS.Bytes += int64(len(data))
		default:
			switch kind {
			case "code":
				st.Code.Count++
				st.Code.Bytes += int64(len(data))
			case "img":
				st.Img.Count++
				st.Img.Bytes += int64(len(data))
			default:
				st.Other.Count++
				st.Other.Bytes += int64(len(data))
			}
		}

		if err := os.WriteFile(dst, data, 0o644); err != nil {
			return st, fmt.Errorf("запись %s: %w", rel, err)
		}
		st.Files++
		st.Bytes += int64(len(data))
	}
	return st, nil
}

// kindOf определяет тип файла по расширению и, для бесширинных имён, по содержимому.
func kindOf(rel string, data []byte) string {
	ext := strings.ToLower(filepath.Ext(rel))
	switch {
	case htmlExt[ext]:
		return "html"
	case cssExt[ext]:
		return "css"
	case codeExt[ext]:
		return "code"
	case imgExt[ext]:
		return "img"
	}
	head := data
	if len(head) > 512 {
		head = head[:512]
	}
	low := strings.ToLower(strings.TrimSpace(string(head)))
	if strings.HasPrefix(low, "<!doctype html") || strings.HasPrefix(low, "<html") {
		return "html"
	}
	return "other"
}

func baseURL(original, primary, rel string) *url.URL {
	if original != "" {
		if u, err := url.Parse(original); err == nil && u.Host != "" {
			return u
		}
	}
	u, err := url.Parse("http://" + primary + "/" + rel)
	if err != nil {
		return nil
	}
	return u
}

// FormatBytes — человекочитаемый размер.
func FormatBytes(n int64) string {
	units := []string{"Б", "КБ", "МБ", "ГБ", "ТБ", "ПБ", "ЭБ"}
	if n < 1024 {
		return fmt.Sprintf("%d Б", n)
	}
	v := float64(n)
	i := 0
	for v >= 1024 && i < len(units)-1 {
		v /= 1024
		i++
	}
	return fmt.Sprintf("%.1f %s", v, units[i])
}
