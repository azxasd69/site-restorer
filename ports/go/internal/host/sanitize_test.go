package host

import "testing"

func TestSanitizeForms(t *testing.T) {
	cases := []struct {
		name string
		in   string
		want string
		note Note
		ok   bool
	}{
		{"url со схемой", "https://prokupol.ru/", "prokupol.ru", NotePlain, true},
		{"url без схемы", "prokupol.ru", "prokupol.ru", NotePlain, true},
		{"путь и порт", "http://prokupol.ru:8080/catalog/tovar?x=1", "prokupol.ru", NotePlain, true},
		{"точка на конце", "prokupol.ru.", "prokupol.ru", NotePlain, true},
		{"текст вокруг", "Сайт: prokupol.ru — магазин", "prokupol.ru", NotePlain, true},
		{"ссылка веб-архива", "https://web.archive.org/web/20261016223707/https://prokupol.ru/", "prokupol.ru", NoteArchive, true},
		{"архив с id_", "http://web.archive.org/web/20261016223707id_/http://prokupol.ru/catalog", "prokupol.ru", NoteArchive, true},
		{"ссылка поиска", "https://www.google.com/url?q=https%3A%2F%2Fprokupol.ru%2F", "prokupol.ru", NoteSearch, true},
		{"русская раскладка", "зкщлгзщдюкг", "prokupol.ru", NoteLayout, true},
		{"кириллический IDN", "прокупол.рф", "xn--j1achbcce0a.xn--p1ai", NoteIDN, true},
		{"IDN с путём", "https://прокупол.рф/catalog/", "xn--j1achbcce0a.xn--p1ai", NoteIDN, true},
		{"верхний регистр", "HTTPS://PROKUPOL.RU/Path", "prokupol.ru", NotePlain, true},
		{"протокол-относительный", "//prokupol.ru/x", "prokupol.ru", NotePlain, true},
		{"пробел в пути", "https://prokupol.ru/a b", "prokupol.ru", NotePlain, true},
		{"слово без точки", "prokupol", "", "", false},
		{"пустая строка", "", "", "", false},
		{"одни пробелы", "   ", "", "", false},
		{"только схема", "https://", "", "", false},
		{"пунктуация", "!!!", "", "", false},
	}
	for _, c := range cases {
		got, note, ok := Sanitize(c.in)
		if ok != c.ok {
			t.Fatalf("%s: ok=%v, ожидалось %v (ввод %q)", c.name, ok, c.ok, c.in)
		}
		if !c.ok {
			continue
		}
		if got != c.want {
			t.Errorf("%s: получено %q, ожидалось %q (ввод %q)", c.name, got, c.want, c.in)
		}
		if note != c.note {
			t.Errorf("%s: note=%q, ожидалось %q", c.name, note, c.note)
		}
	}
}

func TestPunycodeKnownValues(t *testing.T) {
	cases := map[string]string{
		"прокупол.рф": "xn--j1achbcce0a.xn--p1ai",
		"рф":          "xn--p1ai",
		"münchen.de":  "xn--mnchen-3ya.de",
		"пример.рф":   "xn--e1afmkfd.xn--p1ai",
	}
	for in, want := range cases {
		got, err := ToASCII(in)
		if err != nil {
			t.Fatalf("%q: ошибка %v", in, err)
		}
		if got != want {
			t.Errorf("%q: получено %q, ожидалось %q", in, got, want)
		}
	}
}

func TestDekeyboard(t *testing.T) {
	if got := dekeyboard("зкщлгзщдюкг"); got != "prokupol.ru" {
		t.Fatalf("раскладка: получено %q", got)
	}
}

func TestValidHostRejects(t *testing.T) {
	bad := []string{"prokupol", "-bad.ru", "bad-.ru", "a..b", "prokupol.c", "про купол.рф", "prokupol.ru/"}
	for _, b := range bad {
		if validHost(b) {
			t.Errorf("%q признан допустимым хостом", b)
		}
	}
}
