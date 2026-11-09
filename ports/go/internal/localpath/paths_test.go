package localpath

import (
	"strings"
	"testing"
)

func TestForURLBasics(t *testing.T) {
	cases := map[string]string{
		"https://prokupol.ru/":                  "index.html",
		"https://prokupol.ru":                   "index.html",
		"https://prokupol.ru/catalog/":          "catalog/index.html",
		"https://prokupol.ru/catalog/tovar":     "catalog/tovar/index.html",
		"https://prokupol.ru/img/logo.png":      "img/logo.png",
		"https://prokupol.ru/a/b/c/d/style.css": "a/b/c/d/style.css",
	}
	for in, want := range cases {
		got, err := ForURL(in)
		if err != nil {
			t.Fatalf("%q: ошибка %v", in, err)
		}
		if got != want {
			t.Errorf("%q: получено %q, ожидалось %q", in, got, want)
		}
	}
}

func TestQuerySuffixDeterministic(t *testing.T) {
	a, err := ForURL("https://prokupol.ru/search?q=стол&page=2")
	if err != nil {
		t.Fatal(err)
	}
	b, _ := ForURL("https://prokupol.ru/search?q=стол&page=2")
	if a != b {
		t.Fatalf("путь не детерминирован: %q != %q", a, b)
	}
	if !strings.HasPrefix(a, "search/") {
		t.Fatalf("нарушена структура пути: %q", a)
	}
	if !strings.Contains(a, "-q") {
		t.Fatalf("нет суффикса с хэшем: %q", a)
	}
	if !strings.HasSuffix(a, ".html") {
		t.Fatalf("потеряно расширение: %q", a)
	}
	c, _ := ForURL("https://prokupol.ru/search?q=стул")
	if a == c {
		t.Fatalf("разные query дали один путь: %q", a)
	}
	// расширение сохраняется перед суффиксом
	d, _ := ForURL("https://prokupol.ru/page.php?id=3")
	if !strings.HasPrefix(d, "page-q") || !strings.HasSuffix(d, ".php") {
		t.Fatalf("суффикс встал не туда: %q", d)
	}
}

func TestTraversalNeutralized(t *testing.T) {
	inputs := []string{
		"https://prokupol.ru/../../etc/passwd",
		"https://prokupol.ru/a/../../../etc/passwd",
		"https://prokupol.ru/%2e%2e%2f%2e%2e%2fetc/passwd",
		"https://prokupol.ru/a/%2E%2E/b",
	}
	for _, in := range inputs {
		got, err := ForURL(in)
		if err != nil {
			t.Fatalf("%q: ошибка %v", in, err)
		}
		for _, seg := range strings.Split(got, "/") {
			if seg == ".." || seg == "." {
				t.Fatalf("%q -> %q: сегмент обхода %q", in, got, seg)
			}
		}
		if strings.HasPrefix(got, "/") || strings.Contains(got, ":") {
			t.Fatalf("%q -> %q: абсолютный путь или двоеточие", in, got)
		}
	}
}

func TestReservedNames(t *testing.T) {
	cases := map[string]string{
		"https://prokupol.ru/CON": "_CON/index.html", "https://prokupol.ru/dir/aux.txt": "dir/_aux.txt",
		"https://prokupol.ru/com1":      "_com1/index.html",
		"https://prokupol.ru/LPT9.html": "_LPT9.html",
		"https://prokupol.ru/nul/":      "_nul/index.html",
	}
	for in, want := range cases {
		got, err := ForURL(in)
		if err != nil {
			t.Fatalf("%q: ошибка %v", in, err)
		}
		if got != want {
			t.Errorf("%q: получено %q, ожидалось %q", in, got, want)
		}
	}
}

func TestForbiddenCharsEscaped(t *testing.T) {
	got, err := ForURL("https://prokupol.ru/a%3Cb%3Ec%22d%5Ce%7Cf%3Fg%2Ah.png")
	if err != nil {
		t.Fatal(err)
	}
	for _, bad := range []string{"<", ">", "\"", "\\", "|", "?", "*", ":"} {
		if strings.Contains(got, bad) {
			t.Fatalf("в пути остался запрещённый символ %q: %q", bad, got)
		}
	}
	if !strings.HasSuffix(got, ".png") {
		t.Fatalf("потеряно расширение: %q", got)
	}
}

func TestLengthLimit(t *testing.T) {
	long := "https://prokupol.ru/" + strings.Repeat("segment/", 40) + strings.Repeat("x", 30) + ".html"
	got, err := ForURL(long)
	if err != nil {
		t.Fatal(err)
	}
	if len(got) > MaxLen {
		t.Fatalf("путь длиннее %d: %d (%q)", MaxLen, len(got), got)
	}
	again, _ := ForURL(long)
	if got != again {
		t.Fatalf("сжатие пути не детерминировано: %q != %q", got, again)
	}
	for _, seg := range strings.Split(got, "/") {
		if len(seg) > MaxSegment {
			t.Fatalf("сегмент длиннее %d: %q", MaxSegment, seg)
		}
	}
}

func TestUnicodeAndSpaces(t *testing.T) {
	got, err := ForURL("https://xn--j1achbcce0a.xn--p1ai/каталог/фото 1.jpg")
	if err != nil {
		t.Fatal(err)
	}
	for _, r := range got {
		if r > 0x7e {
			t.Fatalf("в пути остался не-ASCII символ %q: %q", r, got)
		}
	}
	if strings.Contains(got, " ") {
		t.Fatalf("в пути остался пробел: %q", got)
	}
	if !strings.HasSuffix(got, ".jpg") {
		t.Fatalf("потеряно расширение: %q", got)
	}
}

func TestTrailingDotSegment(t *testing.T) {
	got, err := ForURL("https://prokupol.ru/dir./file.")
	if err != nil {
		t.Fatal(err)
	}
	for _, seg := range strings.Split(got, "/") {
		if strings.HasSuffix(seg, ".") || strings.HasSuffix(seg, " ") {
			t.Fatalf("сегмент с точкой на конце: %q (%q)", seg, got)
		}
	}
}

func TestHashesDiffer(t *testing.T) {
	a := hash8("a=1")
	b := hash8("a=2")
	if a == b || len(a) != 8 {
		t.Fatalf("FNV-1a: %q и %q", a, b)
	}
}

func TestCleanSegmentDirect(t *testing.T) {
	if got := cleanSegment(".."); got != "_" {
		t.Fatalf("cleanSegment(\"..\") = %q", got)
	}
	if got := cleanSegment(""); got != "_" {
		t.Fatalf("cleanSegment(\"\") = %q", got)
	}
	if got := cleanSegment("a/b"); strings.Contains(got, "/") {
		t.Fatalf("слеш не экранирован: %q", got)
	}
}
