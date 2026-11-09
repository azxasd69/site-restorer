// Package inventory собирает список снимков сайта из CDX API веб-архива.
package inventory

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"sort"
	"strconv"
	"strings"
	"time"
)

// DefaultBase — адрес CDX-сервиса.
const DefaultBase = "http://web.archive.org/cdx/search/cdx"

// Item — один снимок из инвентаря.
type Item struct {
	Timestamp string `json:"timestamp"`
	Original  string `json:"original"`
	MimeType  string `json:"mime"`
	Status    string `json:"status"`
}

// Result — итог обхода инвентаря.
type Result struct {
	Items   []Item `json:"items"`
	Pages   int    `json:"pages"`
	Queries int    `json:"queries"`
}

// Options — параметры обхода.
type Options struct {
	Base      string
	Limit     int // 0 — без ограничения
	MaxPages  int // потолок постраничного обхода (по умолчанию 200)
	Attempts  int // повторов на один запрос (по умолчанию 4)
	Client    *http.Client
	Logf      func(format string, args ...any)
	PageLimit int // limit для одного запроса CDX
}

func (o *Options) normalize() {
	if o.Base == "" {
		o.Base = DefaultBase
	}
	if o.MaxPages <= 0 {
		o.MaxPages = 200
	}
	if o.PageLimit <= 0 {
		o.PageLimit = 50000
	}
	if o.Attempts <= 0 {
		o.Attempts = 4
	}
	if o.Client == nil {
		o.Client = &http.Client{Timeout: 180 * time.Second}
	}
	if o.Logf == nil {
		o.Logf = func(string, ...any) {}
	}
}

// Collect обходит CDX: пробный запрос showNumPages, затем один общий запрос
// либо постраничный обход. Дубликаты по URL схлопываются, остаётся свежий снимок.
func Collect(ctx context.Context, site string, opts Options) (Result, error) {
	opts.normalize()
	out := Result{}

	pages, err := probePages(ctx, site, opts)
	if err != nil {
		return out, err
	}
	out.Pages = pages
	opts.Logf("CDX: страниц в индексе — %d", pages)

	seen := map[string]Item{}
	add := func(items []Item) {
		for _, it := range items {
			key := dedupeKey(it.Original)
			if key == "" {
				continue
			}
			if prev, ok := seen[key]; !ok || it.Timestamp > prev.Timestamp {
				seen[key] = it
			}
		}
	}

	// Один запрос покрывает до PageLimit строк. Если он не удался (504 на больших
	// доменах — обычное дело) или упёрся в лимит — переходим к постраничному обходу.
	quick := opts
	quick.Attempts = 2 // общий запрос на больших доменах часто падает: не тратим время на повторы
	var single []Item
	items, err := query(ctx, site, quick, nil, quick.PageLimit)
	if err == nil {
		out.Queries++
		single = items
		add(items)
	} else {
		opts.Logf("CDX: общий запрос не удался (%v), перехожу к постраничному обходу", err)
	}

	needPages := err != nil || (pages > 1 && len(single) >= opts.PageLimit)
	if needPages && (opts.Limit == 0 || len(seen) < opts.Limit) {
		max := pages
		if max > opts.MaxPages {
			max = opts.MaxPages
		}
		fails, stall := 0, 0
		for page := 0; page < max; page++ {
			p := page
			rows, perr := query(ctx, site, opts, &p, opts.PageLimit)
			if perr != nil {
				fails++
				opts.Logf("CDX: страница %d не отдалась (%v)", page, perr)
				if fails >= 3 {
					break
				}
				continue
			}
			fails = 0
			out.Queries++
			before := len(seen)
			add(rows)
			opts.Logf("CDX: страница %d — строк %d, всего адресов %d", page, len(rows), len(seen))
			if opts.Limit > 0 && len(seen) >= opts.Limit {
				break
			}
			if len(rows) == 0 {
				break
			}
			if len(seen) == before {
				stall++
				if stall >= 2 {
					break // страницы повторяют уже виденное — дальше нет смысла
				}
			} else {
				stall = 0
			}
		}
	}

	out.Items = make([]Item, 0, len(seen))
	for _, it := range seen {
		out.Items = append(out.Items, it)
	}
	if len(out.Items) == 0 {
		return out, fmt.Errorf("CDX не отдал ни одной записи (страниц в индексе: %d)", pages)
	}
	sort.Slice(out.Items, func(i, j int) bool { return out.Items[i].Original < out.Items[j].Original })
	if opts.Limit > 0 && len(out.Items) > opts.Limit {
		out.Items = out.Items[:opts.Limit]
	}
	return out, nil
}

// probePages спрашивает у CDX число страниц индекса.
// Ответ бывает двух видов: {"pages":912} и [["numpages"],["912"]].
func probePages(ctx context.Context, site string, opts Options) (int, error) {
	q := url.Values{}
	q.Set("url", site)
	q.Set("matchType", "domain")
	q.Set("showNumPages", "true")
	q.Set("output", "json")

	body, err := get(ctx, opts, opts.Base+"?"+q.Encode())
	if err != nil {
		return 0, err
	}
	if n, ok := findNumber(body); ok {
		return n, nil
	}
	return 1, nil
}

func findNumber(body []byte) (int, bool) {
	var v any
	if err := json.Unmarshal(body, &v); err != nil {
		// не JSON — считаем, что индекс не разбит на страницы
		return 0, false
	}
	var walk func(any) (int, bool)
	walk = func(x any) (int, bool) {
		switch t := x.(type) {
		case float64:
			return int(t), true
		case string:
			n, err := strconv.Atoi(strings.TrimSpace(t))
			return n, err == nil
		case []any:
			for _, e := range t {
				if n, ok := walk(e); ok {
					return n, true
				}
			}
		case map[string]any:
			for _, k := range []string{"pages", "numpages", "numPages"} {
				if e, ok := t[k]; ok {
					if n, ok := walk(e); ok {
						return n, true
					}
				}
			}
		}
		return 0, false
	}
	return walk(v)
}

// query делает один запрос списка снимков.
func query(ctx context.Context, site string, opts Options, page *int, limit int) ([]Item, error) {
	q := url.Values{}
	q.Set("url", site)
	q.Set("matchType", "domain")
	q.Set("output", "json")
	q.Set("fl", "timestamp,original,mimetype,statuscode")
	q.Set("filter", "statuscode:200")
	q.Set("collapse", "urlkey")
	if limit > 0 {
		q.Set("limit", strconv.Itoa(limit))
	}
	if page != nil {
		q.Set("page", strconv.Itoa(*page))
	}

	body, err := get(ctx, opts, opts.Base+"?"+q.Encode())
	if err != nil {
		return nil, err
	}
	return parseRows(body)
}

func parseRows(body []byte) ([]Item, error) {
	trimmed := strings.TrimSpace(string(body))
	if trimmed == "" || trimmed == "[]" {
		return nil, nil
	}
	var rows [][]any
	if err := json.Unmarshal(body, &rows); err != nil {
		snippet := trimmed
		if len(snippet) > 200 {
			snippet = snippet[:200]
		}
		return nil, fmt.Errorf("CDX вернул не JSON: %s", snippet)
	}
	items := make([]Item, 0, len(rows))
	for _, row := range rows {
		if len(row) < 4 {
			continue
		}
		get := func(i int) string {
			switch v := row[i].(type) {
			case string:
				return v
			case nil:
				return ""
			default:
				return fmt.Sprint(v)
			}
		}
		if get(0) == "timestamp" {
			continue // строка заголовка
		}
		items = append(items, Item{
			Timestamp: get(0),
			Original:  get(1),
			MimeType:  get(2),
			Status:    get(3),
		})
	}
	return items, nil
}

// dedupeKey нормализует URL: схема и хост в нижнем регистре, без порта по
// умолчанию, без фрагмента, пустой путь -> "/".
func dedupeKey(raw string) string {
	u, err := url.Parse(strings.TrimSpace(raw))
	if err != nil || u.Host == "" {
		return ""
	}
	host := strings.ToLower(u.Host)
	host = strings.TrimSuffix(host, ":80")
	host = strings.TrimSuffix(host, ":443")
	path := u.EscapedPath()
	if path == "" {
		path = "/"
	}
	return host + path + "?" + u.RawQuery
}

// get выполняет запрос с повторами на 429/5xx и уважением Retry-After.
func get(ctx context.Context, opts Options, target string) ([]byte, error) {
	var lastErr error
	for attempt := 0; attempt < opts.Attempts; attempt++ {
		if attempt > 0 {
			time.Sleep(time.Duration(1<<attempt) * time.Second)
		}
		req, err := http.NewRequestWithContext(ctx, http.MethodGet, target, nil)
		if err != nil {
			return nil, err
		}
		req.Header.Set("User-Agent", "site-restorer-go/1.0 (+cdx inventory)")
		resp, err := opts.Client.Do(req)
		if err != nil {
			lastErr = err
			continue
		}
		body, readErr := io.ReadAll(resp.Body)
		resp.Body.Close()
		if readErr != nil {
			lastErr = readErr
			continue
		}
		if resp.StatusCode == http.StatusOK {
			return body, nil
		}
		lastErr = fmt.Errorf("CDX ответил %d", resp.StatusCode)
		if resp.StatusCode == http.StatusTooManyRequests || resp.StatusCode >= 500 {
			if d, ok := retryAfter(resp.Header.Get("Retry-After")); ok {
				select {
				case <-ctx.Done():
					return nil, ctx.Err()
				case <-time.After(d):
				}
			}
			continue
		}
		return nil, lastErr
	}
	return nil, lastErr
}

func retryAfter(v string) (time.Duration, bool) {
	v = strings.TrimSpace(v)
	if v == "" {
		return 0, false
	}
	if n, err := strconv.Atoi(v); err == nil && n >= 0 {
		return time.Duration(n) * time.Second, true
	}
	if t, err := http.ParseTime(v); err == nil {
		if d := time.Until(t); d > 0 {
			return d, true
		}
	}
	return 0, false
}
