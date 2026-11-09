// Site Restorer (Go): копия сайта из веб-архива.
//
// Полный конвейер: разбор адреса -> инвентарь CDX -> загрузка -> сборка -> проверка -> отчёт.
package main

import (
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"os"
	"os/signal"
	"path/filepath"
	"sort"
	"strings"
	"syscall"
	"time"

	"siterestorer/internal/assemble"
	"siterestorer/internal/downloader"
	"siterestorer/internal/host"
	"siterestorer/internal/inventory"
	"siterestorer/internal/report"
	"siterestorer/internal/verify"
)

const version = "1.0"

type inventoryFile struct {
	Site      string           `json:"site"`
	Pages     int              `json:"pages"`
	Queries   int              `json:"queries"`
	Collected string           `json:"collected"`
	Items     []inventory.Item `json:"items"`
}

type downloadRecord struct {
	Rel       string `json:"rel"`
	Original  string `json:"original"`
	Timestamp string `json:"timestamp"`
	Mime      string `json:"mime"`
	Status    string `json:"status"`
	Bytes     int64  `json:"bytes"`
	Error     string `json:"error,omitempty"`
}

type downloadFile struct {
	Site    string           `json:"site"`
	Started string           `json:"started"`
	Records []downloadRecord `json:"records"`
}

func main() {
	var (
		cliMode     = flag.Bool("cli", false, "режим командной строки (полный конвейер)")
		siteArg     = flag.String("site", "", "адрес сайта в любом виде")
		outDir      = flag.String("out", "sr-out", "папка результата")
		limit       = flag.Int("limit", 0, "сколько адресов брать из инвентаря (0 — все)")
		workers     = flag.Int("workers", 2, "стартовая конкурентность загрузки")
		maxWorkers  = flag.Int("max-workers", 24, "потолок конкурентности (AIMD)")
		maxPages    = flag.Int("pages", 200, "максимум страниц обхода CDX")
		stage       = flag.String("stage", "all", "этап: all|inventory|download|build|verify|report")
		only        = flag.String("sanitize", "", "только разобрать адрес и выйти")
		cdxBase     = flag.String("cdx", inventory.DefaultBase, "адрес CDX API")
		archiveBase = flag.String("archive", "http://web.archive.org", "адрес веб-архива для загрузки")
	)
	flag.Usage = usage
	flag.Parse()

	if *only != "" {
		h, note, ok := host.Sanitize(*only)
		if !ok {
			fmt.Fprintf(os.Stderr, "не разобрал адрес: %q\n", *only)
			os.Exit(2)
		}
		fmt.Printf("%s\t%s\n", h, note)
		return
	}
	if strings.TrimSpace(*siteArg) == "" {
		usage()
		os.Exit(2)
	}

	hostName, note, ok := host.Sanitize(*siteArg)
	if !ok {
		fmt.Fprintf(os.Stderr, "не разобрал адрес: %q\n", *siteArg)
		os.Exit(2)
	}
	_ = cliMode

	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	started := time.Now()
	if err := os.MkdirAll(*outDir, 0o755); err != nil {
		fail("не создал папку %s: %v", *outDir, err)
	}
	logPath := filepath.Join(*outDir, "run.log")
	logFile, err := os.Create(logPath)
	if err != nil {
		fail("не открыл журнал: %v", err)
	}
	defer logFile.Close()
	logOut := io.MultiWriter(os.Stdout, logFile)
	logf := func(format string, args ...any) {
		fmt.Fprintf(logOut, format+"\n", args...)
	}

	mirror := filepath.Join(*outDir, "mirror")
	siteDir := filepath.Join(*outDir, "site")
	logf("site-restorer-go %s", version)
	logf("адрес: %s (%s), папка: %s", hostName, note, *outDir)

	items, invPages := loadOrCollect(ctx, *stage, *outDir, hostName, *cdxBase, *limit, *maxPages, logf)
	if *stage == "inventory" {
		return
	}
	if len(items) == 0 {
		fail("инвентарь пуст: в архиве нет снимков для %s", hostName)
	}

	jobs, origins := buildJobs(items, hostName)
	records := loadDownload(*outDir, *stage, jobs, *archiveBase, *workers, *maxWorkers, logf)
	if *stage == "download" {
		return
	}
	if len(origins) == 0 {
		for _, rec := range records {
			origins[rec.Rel] = rec.Original
		}
	}

	logf("сборка: mirror -> site")
	buildStats, err := assemble.Run(assemble.Options{
		Mirror: mirror, Site: siteDir, Primary: hostName, Origins: origins, Logf: logf,
	})
	if err != nil {
		fail("сборка не удалась: %v", err)
	}
	if *stage == "build" {
		return
	}

	checkStats, err := verify.Run(siteDir)
	if err != nil {
		fail("проверка не удалась: %v", err)
	}
	logf("проверка: страниц %d, внутренних ссылок %d, битых %d",
		checkStats.Pages, checkStats.Internal, checkStats.Broken)

	if *stage == "verify" {
		return
	}

	var okCount, absent, failed int
	for _, rec := range records {
		switch rec.Status {
		case downloader.StatusOK, downloader.StatusHave:
			okCount++
		case downloader.StatusAbsent:
			absent++
		case downloader.StatusError:
			failed++
		}
	}
	coverage := 0.0
	if len(jobs) > 0 {
		coverage = float64(okCount) / float64(len(jobs)) * 100
	}

	notes := []string{
		"Покрытие считается по адресам инвентаря: скачанные и уже имевшиеся файлы делятся на число заданий.",
		"Битые ссылки — внутренние ссылки из HTML, для которых в site/ нет файла (часть могла не попасть в архив или за пределы --limit).",
	}
	if *limit > 0 {
		notes = append(notes, fmt.Sprintf("Работа ограничена --limit %d: в копию попала только часть адресов.", *limit))
	}

	d := report.Data{
		Host: hostName, Source: "web.archive.org — CDX и режим id_",
		Generated: time.Now(), Elapsed: time.Since(started).Round(time.Second),
		Inventory: len(jobs), Downloaded: okCount, Absent: absent, Failed: failed,
		Mirror: buildStats, Build: buildStats, Check: checkStats,
		Coverage: coverage, Notes: notes,
	}
	reportPath := filepath.Join(*outDir, "report.html")
	if err := report.Write(reportPath, d); err != nil {
		fail("отчёт не записался: %v", err)
	}
	_ = invPages

	logf("готово за %s: файлов %d (%s), битых ссылок %d",
		time.Since(started).Round(time.Second), buildStats.Files,
		assemble.FormatBytes(buildStats.Bytes), checkStats.Broken)
	logf("отчёт: %s", reportPath)
	fmt.Printf("OK %s: файлов %d, битых %d, отчёт %s\n", hostName, buildStats.Files, checkStats.Broken, reportPath)
}

func usage() {
	fmt.Fprintf(os.Stderr, `site-restorer-go %s — копия сайта из веб-архива (только stdlib)

  site-restorer-go --cli --site <адрес> [--out <папка>] [--limit N]
  site-restorer-go --sanitize "<что вставили>"

Адрес понимается в любом виде: со схемой и без, с портом и путём, текстом вокруг,
ссылкой веб-архива или поиска, в русской раскладке, кириллицей (IDN).

Этапы: --stage all|inventory|download|build|verify (по умолчанию all).
Результат: <out>/mirror (снимки), <out>/site (копия), <out>/report.html, журнал <out>/run.log.
`, version)
}

func fail(format string, args ...any) {
	fmt.Fprintf(os.Stderr, format+"\n", args...)
	os.Exit(3)
}

// loadOrCollect читает inventory.json либо собирает инвентарь заново.
func loadOrCollect(ctx context.Context, stage, outDir, site, cdx string, limit, maxPages int, logf func(string, ...any)) ([]inventory.Item, int) {
	path := filepath.Join(outDir, "inventory.json")
	if stage != "all" && stage != "inventory" {
		if data, err := os.ReadFile(path); err == nil {
			var f inventoryFile
			if json.Unmarshal(data, &f) == nil && len(f.Items) > 0 {
				logf("инвентарь: прочитан %s (%d адресов)", path, len(f.Items))
				return f.Items, f.Pages
			}
		}
	}

	res, err := inventory.Collect(ctx, site, inventory.Options{
		Base: cdx, Limit: limit, MaxPages: maxPages, Logf: logf,
	})
	if err != nil {
		fail("инвентарь не собрался: %v", err)
	}
	f := inventoryFile{
		Site: site, Pages: res.Pages, Queries: res.Queries,
		Collected: time.Now().Format(time.RFC3339), Items: res.Items,
	}
	if data, err := json.MarshalIndent(f, "", "  "); err == nil {
		os.WriteFile(path, data, 0o644)
	}
	logf("инвентарь: %d адресов, страниц CDX %d", len(res.Items), res.Pages)
	return res.Items, res.Pages
}

// buildJobs делает список загрузки и карту rel -> исходный URL.
// Один файл копии — одна задача: из дублей (http/https, www, порт) берётся самый свежий снимок.
func buildJobs(items []inventory.Item, primary string) ([]downloader.Job, map[string]string) {
	origins := map[string]string{}
	latest := map[string]string{}
	jobs := make([]downloader.Job, 0, len(items))
	pos := map[string]int{}
	for _, it := range items {
		rel, err := assemble.LocalRel(primary, it.Original)
		if err != nil {
			continue
		}
		if prev, ok := latest[rel]; ok {
			if it.Timestamp > prev {
				latest[rel] = it.Timestamp
				origins[rel] = it.Original
				jobs[pos[rel]] = downloader.Job{Timestamp: it.Timestamp, Original: it.Original, Rel: rel, MimeType: it.MimeType}
			}
			continue
		}
		latest[rel] = it.Timestamp
		origins[rel] = it.Original
		pos[rel] = len(jobs)
		jobs = append(jobs, downloader.Job{
			Timestamp: it.Timestamp, Original: it.Original, Rel: rel, MimeType: it.MimeType,
		})
	}
	sort.Slice(jobs, func(i, j int) bool { return jobs[i].Rel < jobs[j].Rel })
	return jobs, origins
}

// loadDownload качает файлы либо читает download.json.
func loadDownload(outDir, stage string, jobs []downloader.Job, base string, workers, maxWorkers int, logf func(string, ...any)) []downloadRecord {
	path := filepath.Join(outDir, "download.json")
	if stage == "build" || stage == "verify" {
		if data, err := os.ReadFile(path); err == nil {
			var f downloadFile
			if json.Unmarshal(data, &f) == nil && len(f.Records) > 0 {
				logf("загрузка: прочитан %s (%d записей)", path, len(f.Records))
				return f.Records
			}
		}
	}

	dl := downloader.New(downloader.Options{
		Base: base, MirrorDir: filepath.Join(outDir, "mirror"),
		Start: workers, Max: maxWorkers, Logf: logf,
	})
	outcomes := dl.Run(context.Background(), jobs)
	records := make([]downloadRecord, 0, len(outcomes))
	for _, o := range outcomes {
		records = append(records, downloadRecord{
			Rel: o.Rel, Original: o.Original, Timestamp: o.Timestamp,
			Mime: o.MimeType, Status: o.Status, Bytes: o.Bytes, Error: o.Err,
		})
	}
	f := downloadFile{Site: "", Started: time.Now().Format(time.RFC3339), Records: records}
	if data, err := json.MarshalIndent(f, "", "  "); err == nil {
		os.WriteFile(path, data, 0o644)
	}
	var ok, absent, failed int
	for _, r := range records {
		switch r.Status {
		case downloader.StatusOK, downloader.StatusHave:
			ok++
		case downloader.StatusAbsent:
			absent++
		default:
			failed++
		}
	}
	logf("загрузка: ок %d, нет в архиве %d, ошибок %d, конкурентность на финише %d",
		ok, absent, failed, dl.Gate().Current())
	return records
}
