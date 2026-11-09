package assemble

import (
	"strings"
	"unicode/utf8"
)

// cp1251Upper — коды 0x80..0xBF таблицы Windows-1251 (0xC0..0xFF — А..я подряд).
var cp1251Upper = [64]rune{
	0x0402, 0x0403, 0x201A, 0x0453, 0x201E, 0x2026, 0x2020, 0x2021,
	0x20AC, 0x2030, 0x0409, 0x2039, 0x040A, 0x040C, 0x040B, 0x040F,
	0x0452, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014,
	0x0098, 0x2122, 0x0459, 0x203A, 0x045A, 0x045C, 0x045B, 0x045F,
	0x00A0, 0x040E, 0x045E, 0x0408, 0x00A4, 0x0490, 0x00A6, 0x00A7,
	0x0401, 0x00A9, 0x0404, 0x00AB, 0x00AC, 0x00AD, 0x00AE, 0x0407,
	0x00B0, 0x00B1, 0x0406, 0x0456, 0x0491, 0x00B5, 0x00B6, 0x00B7,
	0x0451, 0x2116, 0x0454, 0x00BB, 0x0458, 0x0405, 0x0455, 0x0457,
}

// DecodeToUTF8 приводит байты страницы к строке UTF-8.
// Порядок: BOM -> валидный UTF-8 -> объявленная кодировка -> windows-1251.
func DecodeToUTF8(data []byte, isCSS bool) string {
	if len(data) >= 3 && data[0] == 0xEF && data[1] == 0xBB && data[2] == 0xBF {
		data = data[3:]
	}
	if utf8.Valid(data) {
		return string(data)
	}
	switch strings.ToLower(sniffCharset(data, isCSS)) {
	case "koi8-r", "koi8r":
		return koi8rString(data)
	}
	return cp1251String(data)
}

// sniffCharset ищет объявление кодировки в первых килобайтах документа.
func sniffCharset(data []byte, isCSS bool) string {
	head := data
	if len(head) > 4096 {
		head = head[:4096]
	}
	low := strings.ToLower(string(head))
	if i := strings.Index(low, "charset"); i >= 0 {
		rest := strings.TrimLeft(low[i+len("charset"):], " \t\r\n")
		rest = strings.TrimLeft(strings.TrimPrefix(rest, "="), " \t\r\n\"'")
		end := strings.IndexAny(rest, "\"' \t\r\n;>/")
		if end < 0 {
			end = len(rest)
		}
		if v := strings.TrimSpace(rest[:end]); v != "" {
			return v
		}
	}
	if isCSS {
		if i := strings.Index(low, "@charset"); i >= 0 {
			rest := strings.TrimLeft(low[i+len("@charset"):], " \t\"'")
			if end := strings.IndexAny(rest, "\"' \t\r\n;"); end > 0 {
				return rest[:end]
			}
		}
	}
	return ""
}

func cp1251String(data []byte) string {
	var b strings.Builder
	b.Grow(len(data))
	for _, c := range data {
		switch {
		case c < 0x80:
			b.WriteRune(rune(c))
		case c < 0xC0:
			b.WriteRune(cp1251Upper[c-0x80])
		default:
			b.WriteRune(0x0410 + rune(c-0xC0))
		}
	}
	return b.String()
}

// koi8rString декодирует KOI8-R (встречается на старых страницах Рунета).
func koi8rString(data []byte) string {
	var b strings.Builder
	b.Grow(len(data))
	for _, c := range data {
		switch {
		case c < 0x80:
			b.WriteRune(rune(c))
		case c >= 0xC0:
			b.WriteRune(koi8rHigh[c-0xC0])
		default:
			b.WriteRune(koi8rLow[c-0x80])
		}
	}
	return b.String()
}

var koi8rHigh = [64]rune{
	'ю', 'а', 'б', 'ц', 'д', 'е', 'ф', 'г', 'х', 'и', 'й', 'к', 'л', 'м', 'н', 'о',
	'п', 'я', 'р', 'с', 'т', 'у', 'ж', 'в', 'ь', 'ы', 'з', 'ш', 'э', 'щ', 'ч', 'ъ',
	'Ю', 'А', 'Б', 'Ц', 'Д', 'Е', 'Ф', 'Г', 'Х', 'И', 'Й', 'К', 'Л', 'М', 'Н', 'О',
	'П', 'Я', 'Р', 'С', 'Т', 'У', 'Ж', 'В', 'Ь', 'Ы', 'З', 'Ш', 'Э', 'Щ', 'Ч', 'Ъ',
}

var koi8rLow = [64]rune{
	0x2500, 0x2502, 0x250C, 0x2510, 0x2514, 0x2518, 0x251C, 0x2524,
	0x252C, 0x2534, 0x253C, 0x2580, 0x2584, 0x2588, 0x258C, 0x2590,
	0x2591, 0x2592, 0x2593, 0x2320, 0x25A0, 0x2219, 0x221A, 0x2248,
	0x2264, 0x2265, 0x00A0, 0x2321, 0x00B0, 0x00B2, 0x00B7, 0x00F7,
	0x2550, 0x2551, 0x2552, 0x0451, 0x2553, 0x2554, 0x2555, 0x2556,
	0x2557, 0x2558, 0x2559, 0x255A, 0x255B, 0x255C, 0x255D, 0x255E,
	0x255F, 0x2560, 0x2561, 0x0401, 0x2562, 0x2563, 0x2564, 0x2565,
	0x2566, 0x2567, 0x2568, 0x2569, 0x256A, 0x256B, 0x256C, 0x00A9,
}

// normalizeCharset приводит объявления кодировки к utf-8.
func normalizeCharset(doc string, isCSS bool) string {
	doc = replaceCharsetValues(doc)
	if isCSS {
		doc = replaceAtCharset(doc)
	}
	return doc
}

func isSpaceByte(c byte) bool {
	return c == ' ' || c == '\t' || c == '\r' || c == '\n' || c == '\f'
}

// replaceCharsetValues переписывает charset=..., в том числе внутри content.
func replaceCharsetValues(doc string) string {
	lower := strings.ToLower(doc)
	var b strings.Builder
	i := 0
	for {
		j := strings.Index(lower[i:], "charset")
		if j < 0 {
			b.WriteString(doc[i:])
			break
		}
		j += i
		b.WriteString(doc[i:j])

		end := j + len("charset")
		k := end
		for k < len(doc) && isSpaceByte(doc[k]) {
			k++
		}
		if k >= len(doc) || doc[k] != '=' {
			b.WriteString(doc[j:end])
			i = end
			continue
		}
		k++
		for k < len(doc) && isSpaceByte(doc[k]) {
			k++
		}
		var quote byte
		if k < len(doc) && (doc[k] == '"' || doc[k] == '\'') {
			quote = doc[k]
			k++
		}
		for k < len(doc) {
			c := doc[k]
			if quote != 0 {
				if c == quote {
					break
				}
			} else if isSpaceByte(c) || c == ';' || c == '>' || c == '/' || c == '"' || c == '\'' {
				break
			}
			k++
		}
		b.WriteString("charset=")
		if quote != 0 {
			b.WriteByte(quote)
		}
		b.WriteString("utf-8")
		i = k
	}
	return b.String()
}

// replaceAtCharset: @charset "windows-1251"; -> @charset "utf-8";
func replaceAtCharset(doc string) string {
	lower := strings.ToLower(doc)
	i := strings.Index(lower, "@charset")
	if i < 0 {
		return doc
	}
	j := i + len("@charset")
	for j < len(doc) && (doc[j] == ' ' || doc[j] == '\t') {
		j++
	}
	if j < len(doc) && (doc[j] == '"' || doc[j] == '\'') {
		q := doc[j]
		start := j
		end := start + 1
		for end < len(doc) && doc[end] != q {
			end++
		}
		if end < len(doc) {
			return doc[:start] + string(q) + "utf-8" + doc[end:]
		}
	}
	return doc
}
