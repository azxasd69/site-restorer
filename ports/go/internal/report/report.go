// Package report собирает автономный report.html.
package report

import (
	"html/template"
	"os"
	"time"

	"siterestorer/internal/assemble"
	"siterestorer/internal/verify"
)

// Data — всё, что попадает в отчёт.
type Data struct {
	Host       string
	Source     string
	Generated  time.Time
	Elapsed    time.Duration
	Inventory  int
	Downloaded int
	Absent     int
	Failed     int
	Mirror     assemble.Stats
	Build      assemble.Stats
	Check      verify.Stats
	Coverage   float64
	Notes      []string
}

// Write создаёт report.html.
func Write(path string, d Data) error {
	f, err := os.Create(path)
	if err != nil {
		return err
	}
	defer f.Close()
	return tpl.Execute(f, d)
}

var tpl = template.Must(template.New("report").Funcs(template.FuncMap{
	"bytes": assemble.FormatBytes,
	"pct": func(v float64) string {
		return template.HTMLEscapeString(fmtFloat(v))
	},
}).Parse(page))

func fmtFloat(v float64) string {
	neg := ""
	if v < 0 {
		neg = "-"
		v = -v
	}
	whole := int64(v)
	frac := int64((v - float64(whole)) * 10)
	return neg + itoa(whole) + "." + itoa(frac)
}

func itoa(v int64) string {
	if v == 0 {
		return "0"
	}
	var buf [20]byte
	i := len(buf)
	for v > 0 {
		i--
		buf[i] = byte('0' + v%10)
		v /= 10
	}
	return string(buf[i:])
}

const page = `<!DOCTYPE html>
<html lang="ru">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Отчёт о восстановлении {{.Host}}</title>
<style>
  :root { color-scheme: light dark; }
  body { font: 15px/1.55 -apple-system, "Segoe UI", Roboto, sans-serif; margin: 0; padding: 32px; background: #f6f7f9; color: #1c1f24; }
  main { max-width: 980px; margin: 0 auto; }
  h1 { font-size: 26px; margin: 0 0 4px; }
  .sub { color: #666; margin-bottom: 24px; }
  .cards { display: grid; grid-template-columns: repeat(auto-fit, minmax(190px, 1fr)); gap: 14px; margin-bottom: 26px; }
  .card { background: #fff; border: 1px solid #e3e6ea; border-radius: 10px; padding: 14px 16px; }
  .card .k { font-size: 12px; text-transform: uppercase; letter-spacing: .06em; color: #6b7280; }
  .card .v { font-size: 24px; font-weight: 650; margin-top: 6px; }
  .card .h { font-size: 12px; color: #6b7280; margin-top: 2px; }
  table { width: 100%; border-collapse: collapse; background: #fff; border: 1px solid #e3e6ea; border-radius: 10px; overflow: hidden; margin-bottom: 26px; }
  th, td { text-align: left; padding: 9px 14px; border-bottom: 1px solid #eef0f3; }
  th { background: #fafbfc; font-size: 12px; text-transform: uppercase; letter-spacing: .05em; color: #6b7280; }
  tr:last-child td { border-bottom: 0; }
  td.num { text-align: right; font-variant-numeric: tabular-nums; }
  .bar { height: 8px; background: #e9ecef; border-radius: 4px; overflow: hidden; margin-top: 8px; }
  .bar > i { display: block; height: 100%; background: #2f855a; }
  code { background: #eef1f4; padding: 1px 5px; border-radius: 4px; font-size: 13px; }
  .note { color: #4a5568; font-size: 13px; }
  ul.notes { color: #4a5568; font-size: 14px; }
</style>
</head>
<body>
<main>
  <h1>Отчёт: {{.Host}}</h1>
  <div class="sub">Источник: {{.Source}} · собрано {{.Generated.Format "02.01.2006 15:04:05"}} · {{.Elapsed}}</div>

  <div class="cards">
    <div class="card"><div class="k">Покрытие</div><div class="v">{{pct .Coverage}}%</div>
      <div class="h">{{.Downloaded}} из {{.Inventory}} адресов</div>
      <div class="bar"><i style="width: {{pct .Coverage}}%"></i></div></div>
    <div class="card"><div class="k">Размер копии</div><div class="v">{{bytes .Build.Bytes}}</div>
      <div class="h">{{.Build.Files}} файлов</div></div>
    <div class="card"><div class="k">Битые ссылки</div><div class="v">{{.Check.Broken}}</div>
      <div class="h">из {{.Check.Internal}} внутренних</div></div>
    <div class="card"><div class="k">Нет в архиве</div><div class="v">{{.Absent}}</div>
      <div class="h">404 по всем годам · ошибок {{.Failed}}</div></div>
  </div>

  <table>
    <tr><th>Тип</th><th class="num">Файлов</th><th class="num">Размер</th></tr>
    <tr><td>HTML</td><td class="num">{{.Build.HTML.Count}}</td><td class="num">{{bytes .Build.HTML.Bytes}}</td></tr>
    <tr><td>CSS</td><td class="num">{{.Build.CSS.Count}}</td><td class="num">{{bytes .Build.CSS.Bytes}}</td></tr>
    <tr><td>Код и шрифты</td><td class="num">{{.Build.Code.Count}}</td><td class="num">{{bytes .Build.Code.Bytes}}</td></tr>
    <tr><td>Изображения</td><td class="num">{{.Build.Img.Count}}</td><td class="num">{{bytes .Build.Img.Bytes}}</td></tr>
    <tr><td>Прочее</td><td class="num">{{.Build.Other.Count}}</td><td class="num">{{bytes .Build.Other.Bytes}}</td></tr>
  </table>

  <table>
    <tr><th>Проверка ссылок</th><th class="num">Значение</th></tr>
    <tr><td>HTML-страниц проверено</td><td class="num">{{.Check.Pages}}</td></tr>
    <tr><td>Внутренних ссылок</td><td class="num">{{.Check.Internal}}</td></tr>
    <tr><td>Битых</td><td class="num">{{.Check.Broken}}</td></tr>
    <tr><td>Внешних (не трогали)</td><td class="num">{{.Check.External}}</td></tr>
    <tr><td>Переписано при сборке</td><td class="num">{{.Build.Rewrote}}</td></tr>
    <tr><td>Ссылок без файла в копии</td><td class="num">{{.Build.Missing}}</td></tr>
  </table>

  {{if .Check.Samples}}
  <table>
    <tr><th>Примеры битых ссылок</th><th>Страница</th><th>Цель</th></tr>
    {{range .Check.Samples}}
    <tr><td><code>{{.Href}}</code></td><td>{{.Page}}</td><td><code>{{.Target}}</code></td></tr>
    {{end}}
  </table>
  {{end}}

  {{if .Notes}}
  <ul class="notes">{{range .Notes}}<li>{{.}}</li>{{end}}</ul>
  {{end}}
  <p class="note">Отчёт создан утилитой site-restorer-go. Файлы сайта — в папке <code>site/</code>, исходные снимки — в <code>mirror/</code>.</p>
</main>
</body>
</html>
`
