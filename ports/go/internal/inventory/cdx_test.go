package inventory

import "testing"

func TestFindNumber(t *testing.T) {
	cases := map[string]int{
		`[["numpages"],["912"]]`: 912,
		`{"pages": 3}`:           3,
		`{"numpages":"7"}`:       7,
	}
	for body, want := range cases {
		got, ok := findNumber([]byte(body))
		if !ok || got != want {
			t.Errorf("%s: получено %d (ok=%v), ожидалось %d", body, got, ok, want)
		}
	}
	if _, ok := findNumber([]byte("<html>offline</html>")); ok {
		t.Error("HTML-ошибка не должна давать число страниц")
	}
}

func TestParseRows(t *testing.T) {
	body := `[["timestamp","original","mimetype","statuscode"],
["20261016223707","https://site.ru/","text/html","200"],
["20260101000000","https://site.ru/","text/html","200"],
["20250101000000","https://site.ru/about","text/html","200"]]`
	items, err := parseRows([]byte(body))
	if err != nil {
		t.Fatal(err)
	}
	if len(items) != 3 {
		t.Fatalf("строк: %d, ожидалось 3", len(items))
	}
	if items[0].Timestamp != "20261016223707" || items[0].Original != "https://site.ru/" {
		t.Errorf("разбор полей: %+v", items[0])
	}

	empty, err := parseRows([]byte("  "))
	if err != nil || len(empty) != 0 {
		t.Errorf("пустой ответ: %v, %d", err, len(empty))
	}

	if _, err := parseRows([]byte("<html>offline</html>")); err == nil {
		t.Error("HTML вместо JSON должен быть ошибкой")
	}
}

func TestDedupeKey(t *testing.T) {
	cases := [][2]string{
		{"https://site.ru/", "http://site.ru/"},
		{"http://site.ru:80/a", "http://site.ru/a"},
		{"http://site.ru/a#frag", "http://site.ru/a"},
	}
	for _, c := range cases {
		if dedupeKey(c[0]) != dedupeKey(c[1]) {
			t.Errorf("ключи различаются: %q (%q) и %q (%q)", c[0], dedupeKey(c[0]), c[1], dedupeKey(c[1]))
		}
	}
	if dedupeKey("http://site.ru/a") == dedupeKey("http://site.ru/b") {
		t.Error("разные пути дали один ключ")
	}
}
