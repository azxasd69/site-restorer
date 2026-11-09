// Package downloader качает снимки из веб-архива (режим id_) с адаптивной
// конкурентностью AIMD и докачкой по факту наличия файла.
package downloader

import (
	"context"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

// Статусы одного задания.
const (
	StatusOK     = "ok"     // скачано сейчас
	StatusHave   = "have"   // файл уже был и непустой
	StatusAbsent = "absent" // в архиве нет (404 по всем годам) — не ошибка
	StatusError  = "error"  // сетевая ошибка или 5xx после повторов
)

// Job — один файл к загрузке.
type Job struct {
	Timestamp string
	Original  string
	Rel       string
	MimeType  string
}

// Outcome — итог по одному заданию.
type Outcome struct {
	Job
	Status string
	Bytes  int64
	Err    string
}

// Options — настройки загрузчика.
type Options struct {
	Base       string // http://web.archive.org
	MirrorDir  string
	Start      int // стартовая конкурентность (2)
	Max        int // потолок (24)
	Retries    int // повторов на сетевую ошибку
	Client     *http.Client
	Logf       func(format string, args ...any)
	PauseAfter int           // сколько 429 подряд до паузы (3)
	PauseFor   time.Duration // длительность паузы (120 с)
}

func (o *Options) normalize() {
	if o.Base == "" {
		o.Base = "http://web.archive.org"
	}
	if o.Start < 1 {
		o.Start = 2
	}
	if o.Max < o.Start {
		o.Max = 24
	}
	if o.Retries <= 0 {
		o.Retries = 3
	}
	if o.PauseAfter <= 0 {
		o.PauseAfter = 3
	}
	if o.PauseFor <= 0 {
		o.PauseFor = 120 * time.Second
	}
	if o.Client == nil {
		o.Client = &http.Client{Timeout: 120 * time.Second}
	}
	if o.Logf == nil {
		o.Logf = func(string, ...any) {}
	}
}

// Gate — семафор с изменяемой ёмкостью и паузой.
type Gate struct {
	mu          sync.Mutex
	cond        *sync.Cond
	cur         int
	max         int
	inflight    int
	pausedUntil time.Time
}

// NewGate создаёт семафор с текущей и максимальной ёмкостью.
func NewGate(start, max int) *Gate {
	g := &Gate{cur: start, max: max}
	g.cond = sync.NewCond(&g.mu)
	return g
}

// Acquire занимает слот, дожидаясь паузы и свободной ёмкости.
func (g *Gate) Acquire() {
	for {
		g.mu.Lock()
		for g.inflight >= g.cur {
			g.cond.Wait()
		}
		wait := time.Until(g.pausedUntil)
		if wait > 0 {
			g.mu.Unlock()
			if wait > time.Second {
				wait = time.Second
			}
			time.Sleep(wait)
			continue
		}
		g.inflight++
		g.mu.Unlock()
		return
	}
}

// Release освобождает слот.
func (g *Gate) Release() {
	g.mu.Lock()
	if g.inflight > 0 {
		g.inflight--
	}
	g.cond.Broadcast()
	g.mu.Unlock()
}

// Set меняет ёмкость (в пределах 1..max).
func (g *Gate) Set(n int) {
	g.mu.Lock()
	if n < 1 {
		n = 1
	}
	if n > g.max {
		n = g.max
	}
	g.cur = n
	g.cond.Broadcast()
	g.mu.Unlock()
}

// Current возвращает текущую ёмкость.
func (g *Gate) Current() int {
	g.mu.Lock()
	defer g.mu.Unlock()
	return g.cur
}

// Max возвращает потолок.
func (g *Gate) Max() int {
	g.mu.Lock()
	defer g.mu.Unlock()
	return g.max
}

// Pause останавливает выдачу слотов на d.
func (g *Gate) Pause(d time.Duration) {
	g.mu.Lock()
	if until := time.Now().Add(d); until.After(g.pausedUntil) {
		g.pausedUntil = until
	}
	g.cond.Broadcast()
	g.mu.Unlock()
}

// Downloader — исполнитель загрузки.
type Downloader struct {
	opts  Options
	gate  *Gate
	done  int64
	mu    sync.Mutex
	count map[string]int
}

// New собирает загрузчик.
func New(opts Options) *Downloader {
	opts.normalize()
	return &Downloader{
		opts:  opts,
		gate:  NewGate(opts.Start, opts.Max),
		count: map[string]int{},
	}
}

// Gate открывает семафор наружу (для отчёта о конкурентности).
func (d *Downloader) Gate() *Gate { return d.gate }

// Run выполняет все задания и возвращает результаты в исходном порядке.
func (d *Downloader) Run(ctx context.Context, jobs []Job) []Outcome {
	out := make([]Outcome, len(jobs))
	if len(jobs) == 0 {
		return out
	}

	ctx, cancel := context.WithCancel(ctx)
	defer cancel()

	var wg sync.WaitGroup
	adaptDone := make(chan struct{})
	go func() {
		defer close(adaptDone)
		d.adapt(ctx)
	}()

	idx := make(chan int)
	workers := d.opts.Max
	for i := 0; i < workers; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for n := range idx {
				res := d.fetch(ctx, jobs[n])
				out[n] = res
				d.report(res)
			}
		}()
	}
	for i := range jobs {
		select {
		case <-ctx.Done():
		case idx <- i:
		}
	}
	close(idx)
	wg.Wait()
	cancel()
	<-adaptDone
	return out
}

func (d *Downloader) report(res Outcome) {
	n := atomic.AddInt64(&d.done, 1)
	d.mu.Lock()
	d.count[res.Status]++
	cur := d.gate.Current()
	d.mu.Unlock()
	if n%25 == 0 || res.Status == StatusError {
		d.opts.Logf("загрузка: %d обработано (ок=%d, ошибок=%d), конкурентность %d",
			n, d.count[StatusOK], d.count[StatusError], cur)
	}
}

// adapt — AIMD: +1 каждые 3 с, если за окно что-то скачалось.
func (d *Downloader) adapt(ctx context.Context) {
	t := time.NewTicker(3 * time.Second)
	defer t.Stop()
	var prev int64
	for {
		select {
		case <-ctx.Done():
			return
		case <-t.C:
			cur := atomic.LoadInt64(&d.done)
			if cur > prev && d.gate.Current() < d.gate.Max() {
				d.gate.Set(d.gate.Current() + 1)
			}
			prev = cur
		}
	}
}

// fetch качает одно задание: 200 -> файл, 404 -> соседние годы, иначе ошибка.
func (d *Downloader) fetch(ctx context.Context, job Job) Outcome {
	out := Outcome{Job: job}
	dest := filepath.Join(d.opts.MirrorDir, filepath.FromSlash(job.Rel))

	if st, err := os.Stat(dest); err == nil && st.Size() > 0 {
		out.Status = StatusHave
		out.Bytes = st.Size()
		return out
	}

	if err := os.MkdirAll(filepath.Dir(dest), 0o755); err != nil {
		out.Status = StatusError
		out.Err = err.Error()
		return out
	}

	var lastErr string
	for _, ts := range candidateStamps(job.Timestamp) {
		target := fmt.Sprintf("%s/web/%sid_/%s", strings.TrimRight(d.opts.Base, "/"), ts, job.Original)
		n, status := d.downloadOne(ctx, target, dest)
		switch status {
		case StatusOK:
			out.Status = StatusOK
			out.Bytes = n
			return out
		case StatusAbsent:
			continue // пробуем предыдущий год
		default:
			lastErr = fmt.Sprintf("снимок %s: %s", ts, status)
		}
	}
	if lastErr == "" {
		out.Status = StatusAbsent
		return out
	}
	out.Status = StatusError
	out.Err = lastErr
	return out
}

func (d *Downloader) downloadOne(ctx context.Context, target, dest string) (int64, string) {
	var lastErr string
	for attempt := 0; attempt <= d.opts.Retries; attempt++ {
		if attempt > 0 {
			time.Sleep(time.Duration(attempt) * time.Second)
		}
		d.gate.Acquire()
		req, err := http.NewRequestWithContext(ctx, http.MethodGet, target, nil)
		if err != nil {
			d.gate.Release()
			return 0, StatusError
		}
		req.Header.Set("User-Agent", "site-restorer-go/1.0 (+restore)")

		resp, err := d.opts.Client.Do(req)
		if err != nil {
			d.gate.Release()
			lastErr = "сеть: " + err.Error()
			continue
		}

		switch {
		case resp.StatusCode == http.StatusOK:
			n, werr := writeFile(dest, resp.Body)
			resp.Body.Close()
			d.gate.Release()
			if werr != nil {
				return 0, "запись: " + werr.Error()
			}
			d.onOK()
			return n, StatusOK
		case resp.StatusCode == http.StatusNotFound:
			resp.Body.Close()
			d.gate.Release()
			return 0, StatusAbsent
		case resp.StatusCode == http.StatusTooManyRequests:
			wait, hasWait := retryAfter(resp.Header.Get("Retry-After"))
			resp.Body.Close()
			d.gate.Release()
			d.on429()
			if hasWait {
				d.opts.Logf("429: ждём %s по Retry-After", wait)
				sleepCtx(ctx, wait)
			}
			lastErr = "429 Too Many Requests"
			continue
		case resp.StatusCode >= 500:
			resp.Body.Close()
			d.gate.Release()
			lastErr = fmt.Sprintf("%d %s", resp.StatusCode, resp.Status)
			continue
		default:
			resp.Body.Close()
			d.gate.Release()
			lastErr = fmt.Sprintf("%d %s", resp.StatusCode, resp.Status)
			// только 404 означает «нет в архиве»; остальное — ошибка
			return 0, StatusError
		}
	}
	if lastErr == "" {
		lastErr = "сеть: нет ответа"
	}
	return 0, lastErr
}

// on429 уменьшает конкурентность вдвое; три подряд — пауза и сброс к 2.
func (d *Downloader) on429() {
	d.gate.Set(d.gate.Current() / 2)
	d.mu.Lock()
	d.count["429"]++
	streak := d.count["429"]
	d.mu.Unlock()
	if streak >= d.opts.PauseAfter {
		d.mu.Lock()
		d.count["429"] = 0
		d.mu.Unlock()
		d.gate.Set(2)
		d.opts.Logf("429 трижды подряд: пауза %s, конкурентность сброшена к 2", d.opts.PauseFor)
		d.gate.Pause(d.opts.PauseFor)
	}
}

func (d *Downloader) onOK() {
	d.mu.Lock()
	d.count["429"] = 0
	d.mu.Unlock()
}

func writeFile(dest string, r io.Reader) (int64, error) {
	part := dest + ".part"
	f, err := os.Create(part)
	if err != nil {
		return 0, err
	}
	n, err := io.Copy(f, r)
	if cerr := f.Close(); err == nil {
		err = cerr
	}
	if err != nil {
		os.Remove(part)
		return n, err
	}
	if n == 0 {
		os.Remove(part)
		return 0, fmt.Errorf("пустой ответ")
	}
	return n, os.Rename(part, dest)
}

// candidateStamps: текущий год, затем -1 и -2 (тот же месяц/день/время).
func candidateStamps(ts string) []string {
	out := []string{ts}
	if len(ts) < 8 {
		return out
	}
	year, err := strconv.Atoi(ts[:4])
	if err != nil {
		return out
	}
	for k := 1; k <= 2; k++ {
		cand := fmt.Sprintf("%04d%s", year-k, ts[4:])
		if len(cand) != len(ts) {
			continue
		}
		if _, err := time.Parse("20060102150405", cand); err == nil {
			out = append(out, cand)
		}
	}
	return out
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

func sleepCtx(ctx context.Context, d time.Duration) {
	if d <= 0 {
		return
	}
	select {
	case <-ctx.Done():
	case <-time.After(d):
	}
}
