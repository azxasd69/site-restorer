package assemble

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func writeFile(t *testing.T, root, rel string, data []byte) {
	t.Helper()
	p := filepath.Join(root, filepath.FromSlash(rel))
	if err := os.MkdirAll(filepath.Dir(p), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(p, data, 0o644); err != nil {
		t.Fatal(err)
	}
}

func build(t *testing.T, files map[string]string) (string, Stats) {
	t.Helper()
	root := t.TempDir()
	mirror := filepath.Join(root, "mirror")
	site := filepath.Join(root, "site")
	origins := map[string]string{}
	for rel, body := range files {
		writeFile(t, mirror, rel, []byte(body))
		origins[rel] = "https://site.ru/" + rel
	}
	st, err := Run(Options{Mirror: mirror, Site: site, Primary: "site.ru", Origins: origins})
	if err != nil {
		t.Fatalf("сборка: %v", err)
	}
	return site, st
}

func read(t *testing.T, site, rel string) string {
	t.Helper()
	data, err := os.ReadFile(filepath.Join(site, filepath.FromSlash(rel)))
	if err != nil {
		t.Fatalf("чтение %s: %v", rel, err)
	}
	return string(data)
}

func TestRewriteLinks(t *testing.T) {
	page := `<html><head><base href="https://site.ru/">
<link rel="stylesheet" href="/css/style.css">
<meta charset="windows-1251"></head>
<body style="background: url(/img/bg.png)">
<a href="https://site.ru/about/">О нас</a>
<a href='/contacts/'>Контакты</a>
<a href=/catalog/unquoted>Каталог</a>
<a href="mailto:info@site.ru">почта</a>
<a href="https://other.example/x">внешняя</a>
<img src="img/logo.png" srcset="img/logo.png 1x, img/logo@2x.png 2x">
<img data-src="img/lazy.png" poster="img/poster.jpg">
</body></html>`
	site, st := build(t, map[string]string{
		"index.html":                  page,
		"about/index.html":            "<html><body>about</body></html>",
		"contacts/index.html":         "<html><body>contacts</body></html>",
		"catalog/unquoted/index.html": "<html><body>catalog</body></html>",
		"css/style.css":               "body { background: url(/img/bg.png); }",
		"img/logo.png":                "png",
		"img/logo@2x.png":             "png2",
		"img/lazy.png":                "lazy",
		"img/poster.jpg":              "poster",
		"img/bg.png":                  "bg",
	})

	got := read(t, site, "index.html")
	for _, want := range []string{
		`href="about/index.html"`,
		`href='contacts/index.html'`,
		`catalog/unquoted/index.html`,
		`src="img/logo.png"`,
		`data-src="img/lazy.png"`,
		`poster="img/poster.jpg"`,
		`url(img/bg.png)`,
		`srcset="img/logo.png 1x, img/logo@2x.png 2x"`,
	} {
		if !strings.Contains(got, want) {
			t.Errorf("нет фрагмента %q\n---\n%s", want, got)
		}
	}
	if strings.Contains(got, "https://site.ru") || strings.Contains(got, "href=\"/") {
		t.Errorf("остались абсолютные ссылки на свой домен:\n%s", got)
	}
	if strings.Contains(strings.ToLower(got), "<base") {
		t.Errorf("<base> не удалён:\n%s", got)
	}
	if strings.Contains(got, "charset=windows-1251") || strings.Contains(got, "charset=\"windows-1251\"") {
		t.Errorf("кодировка не приведена к utf-8:\n%s", got)
	}
	if !strings.Contains(got, `href="mailto:info@site.ru"`) {
		t.Errorf("mailto испорчен:\n%s", got)
	}
	if !strings.Contains(got, `href="https://other.example/x"`) {
		t.Errorf("внешняя ссылка изменена:\n%s", got)
	}
	if !strings.Contains(read(t, site, "css/style.css"), "url(../img/bg.png)") {
		t.Errorf("CSS не переписан: %s", read(t, site, "css/style.css"))
	}
	if st.Links == 0 || st.Rewrote == 0 {
		t.Errorf("статистика ссылок пуста: %+v", st)
	}
}

func TestBitrixHeuristics(t *testing.T) {
	page := `<html><head>
<link rel="stylesheet" href="/bitrix/cache/css/s1/main/page/styles_ffffeeee_v1.css">
</head><body>
<img src="/upload/resize_cache/iblock/a1/200_200/photo.jpg">
<img src="/upload/resize_cache/iblock/b2/100_100/missing.jpg">
</body></html>`
	site, st := build(t, map[string]string{
		"index.html": page,
		"bitrix/cache/css/s1/main/page/styles_1234abcd_v1.css": "body{color:red}",
		"upload/iblock/a1/photo.jpg":                           "jpg",
	})
	got := read(t, site, "index.html")
	if !strings.Contains(got, `href="bitrix/cache/css/s1/main/page/styles_1234abcd_v1.css"`) {
		t.Errorf("кэш CSS не сопоставлен: %s", got)
	}
	if !strings.Contains(got, `src="upload/iblock/a1/photo.jpg"`) {
		t.Errorf("resize_cache не сопоставлен: %s", got)
	}
	if !strings.Contains(got, "upload/resize_cache/iblock/b2") {
		t.Errorf("несопоставимый путь должен остаться исходным: %s", got)
	}
	if st.Missing == 0 {
		t.Errorf("отсутствующий файл не учтён: %+v", st)
	}
}

func TestCharsetWindows1251(t *testing.T) {
	// «Привет, мир!» в windows-1251
	body := []byte("<html><head><meta charset=\"windows-1251\"></head><body>")
	for _, r := range "Привет, мир!" {
		if r < 0x80 {
			body = append(body, byte(r))
		} else {
			body = append(body, byte(0xC0+rune(r)-0x0410))
		}
	}
	body = append(body, []byte("</body></html>")...)

	root := t.TempDir()
	mirror := filepath.Join(root, "mirror")
	writeFile(t, mirror, "index.html", body)
	st, err := Run(Options{
		Mirror: mirror, Site: filepath.Join(root, "site"), Primary: "site.ru",
		Origins: map[string]string{"index.html": "http://site.ru/"},
	})
	if err != nil {
		t.Fatal(err)
	}
	if st.HTML.Count != 1 {
		t.Fatalf("HTML не распознан: %+v", st)
	}
	got := read(t, filepath.Join(root, "site"), "index.html")
	if !strings.Contains(got, "Привет, мир!") {
		t.Errorf("cp1251 не декодирован: %q", got)
	}
	if strings.Contains(got, "windows-1251") {
		t.Errorf("объявление кодировки не заменено: %q", got)
	}
	if !strings.Contains(got, "charset=utf-8") && !strings.Contains(got, `charset="utf-8"`) {
		t.Errorf("нет объявления utf-8: %q", got)
	}
}

func TestRelLink(t *testing.T) {
	cases := []struct{ from, to, want string }{
		{"index.html", "about/index.html", "about/index.html"},
		{"about/index.html", "index.html", "../index.html"},
		{"a/b/index.html", "a/c/style.css", "../c/style.css"},
		{"a/index.html", "a/b/index.html", "b/index.html"},
		{"deep/x/y.html", "img/logo.png", "../../img/logo.png"},
	}
	for _, c := range cases {
		if got := RelLink(c.from, c.to); got != c.want {
			t.Errorf("RelLink(%q,%q)=%q, ожидалось %q", c.from, c.to, got, c.want)
		}
	}
}

func TestCanonicalHost(t *testing.T) {
	if !SameSite("example.com", "www.example.com") {
		t.Error("www не считается тем же сайтом")
	}
	if SameSite("example.com", "cdn.example.com") {
		t.Error("поддомен ошибочно признан основным")
	}
	if CanonicalHost("WWW.Example.COM:443") != "example.com" {
		t.Errorf("канонизация: %q", CanonicalHost("WWW.Example.COM:443"))
	}
}
