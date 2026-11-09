package verify

import (
	"os"
	"path/filepath"
	"testing"
)

func TestExtractLinksQuotes(t *testing.T) {
	doc := `<a href="a.html">1</a><a href='b.html'>2</a><a href=c.html>3</a>
<img src="/img/x.png" srcset="img/y.png 1x, img/z.png 2x">
<div style="background:url('bg.png')"></div>
<a href="#top">4</a><a href="mailto:a@b.ru">5</a><a href="https://ext.example/x">6</a>`
	got := ExtractLinks(doc)
	want := []string{"a.html", "b.html", "c.html", "/img/x.png", "img/y.png", "img/z.png", "bg.png", "#top", "mailto:a@b.ru", "https://ext.example/x"}
	for _, w := range want {
		found := false
		for _, g := range got {
			if g == w {
				found = true
				break
			}
		}
		if !found {
			t.Errorf("не найдена ссылка %q в %v", w, got)
		}
	}
}

func TestRunCountsBroken(t *testing.T) {
	root := t.TempDir()
	pages := map[string]string{
		"index.html":       `<a href="about/index.html">ok</a><a href='missing.html'>bad</a><a href="https://ext.example/">ext</a>`,
		"about/index.html": `<a href="../index.html">home</a><img src="../img/logo.png">`,
		"img/logo.png":     "png",
	}
	for rel, body := range pages {
		p := filepath.Join(root, filepath.FromSlash(rel))
		os.MkdirAll(filepath.Dir(p), 0o755)
		if err := os.WriteFile(p, []byte(body), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	st, err := Run(root)
	if err != nil {
		t.Fatal(err)
	}
	if st.Pages != 2 {
		t.Errorf("страниц: %d, ожидалось 2", st.Pages)
	}
	if st.Internal != 4 {
		t.Errorf("внутренних ссылок: %d, ожидалось 4 (%+v)", st.Internal, st)
	}
	if st.Broken != 1 {
		t.Errorf("битых: %d, ожидалось 1 (%+v)", st.Broken, st)
	}
	if len(st.Samples) != 1 || st.Samples[0].Href != "missing.html" {
		t.Errorf("примеры битых: %+v", st.Samples)
	}
	if st.External != 1 {
		t.Errorf("внешних: %d, ожидалось 1", st.External)
	}
}
