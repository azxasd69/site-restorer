// Package host разбирает то, что пользователь вставил в поле «адрес сайта»,
// и приводит кириллические домены к punycode (RFC 3492) без внешних зависимостей.
package host

import (
	"errors"
	"strings"
)

// Параметры алгоритма Bootstring из RFC 3492.
const (
	pcBase        = 36
	pcTMin        = 1
	pcTMax        = 26
	pcSkew        = 38
	pcDamp        = 700
	pcInitialBias = 72
	pcInitialN    = 128
)

var errPunycode = errors.New("punycode: метка не кодируется")

// adapt — функция смещения (RFC 3492, 6.1).
func adapt(delta, numPoints int, firstTime bool) int {
	if firstTime {
		delta /= pcDamp
	} else {
		delta /= 2
	}
	delta += delta / numPoints
	k := 0
	for delta > ((pcBase-pcTMin)*pcTMax)/2 {
		delta /= pcBase - pcTMin
		k += pcBase
	}
	return k + (pcBase-pcTMin+1)*delta/(delta+pcSkew)
}

func encodeDigit(d int) byte {
	if d < 26 {
		return byte('a' + d)
	}
	return byte('0' + d - 26)
}

// encodeLabel кодирует одну метку домена в punycode (без префикса xn--).
func encodeLabel(label string) (string, error) {
	runes := []rune(label)
	if len(runes) == 0 {
		return "", errPunycode
	}
	var out []byte
	basic := 0
	for _, r := range runes {
		if r < 0x80 {
			out = append(out, byte(r))
			basic++
		}
	}
	h := basic
	if basic > 0 {
		out = append(out, '-')
	}

	n := pcInitialN
	delta := 0
	bias := pcInitialBias
	maxInt := int(^uint(0) >> 1)

	for h < len(runes) {
		m := maxInt
		for _, r := range runes {
			if int(r) >= n && int(r) < m {
				m = int(r)
			}
		}
		if m == maxInt {
			return "", errPunycode
		}
		delta += (m - n) * (h + 1)
		n = m
		for _, r := range runes {
			if int(r) < n {
				delta++
			}
			if int(r) == n {
				q := delta
				for k := pcBase; ; k += pcBase {
					t := k - bias
					if t < pcTMin {
						t = pcTMin
					} else if t > pcTMax {
						t = pcTMax
					}
					if q < t {
						break
					}
					out = append(out, encodeDigit(t+(q-t)%(pcBase-t)))
					q = (q - t) / (pcBase - t)
				}
				out = append(out, encodeDigit(q))
				bias = adapt(delta, h+1, h == basic)
				delta = 0
				h++
			}
		}
		delta++
		n++
	}
	return string(out), nil
}

// ToASCII переводит домен в ASCII-форму: метки с не-ASCII получают префикс xn--.
func ToASCII(domain string) (string, error) {
	labels := strings.Split(domain, ".")
	out := make([]string, 0, len(labels))
	for _, label := range labels {
		if label == "" {
			return "", errPunycode
		}
		ascii := true
		for _, r := range label {
			if r >= 0x80 {
				ascii = false
				break
			}
		}
		if ascii {
			out = append(out, strings.ToLower(label))
			continue
		}
		enc, err := encodeLabel(strings.ToLower(label))
		if err != nil {
			return "", err
		}
		out = append(out, "xn--"+enc)
	}
	return strings.Join(out, "."), nil
}
