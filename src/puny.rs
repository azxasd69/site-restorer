//! Punycode для доменных имён (RFC 3492) — замена крейта `idna`.
//!
//! Крейт `idna` тянет ICU4X (нормализация, коллекции, yoke, zerovec — десятки
//! крейтов) ради полного UTS-46. Нам нужно ровно одно: перевести кириллический
//! домен в `xn--…`, чтобы он годился для запросов в веб-архив. Нормализацию
//! Unicode не выполняем: адрес приходит из поля ввода или из ссылки, где метки
//! уже в NFC; для смешанных случаев остаётся проверка на допустимые символы.

const BASE: u32 = 36;
const TMIN: u32 = 1;
const TMAX: u32 = 26;
const SKEW: u32 = 38;
const DAMP: u32 = 700;
const INITIAL_BIAS: u32 = 72;
const INITIAL_N: u32 = 128;

fn digit(d: u32) -> char {
    if d < 26 {
        (b'a' + d as u8) as char
    } else {
        (b'0' + (d - 26) as u8) as char
    }
}

fn adapt(mut delta: u32, numpoints: u32, first: bool) -> u32 {
    delta = if first { delta / DAMP } else { delta / 2 };
    delta += delta / numpoints;
    let mut k = 0;
    while delta > ((BASE - TMIN) * TMAX) / 2 {
        delta /= BASE - TMIN;
        k += BASE;
    }
    k + ((BASE - TMIN + 1) * delta) / (delta + SKEW)
}

/// Закодировать одну метку в punycode (без префикса `xn--`).
fn encode_label(input: &str) -> Option<String> {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::new();
    let basic: Vec<char> = chars
        .iter()
        .copied()
        .filter(|c| (*c as u32) < 0x80)
        .collect();
    let handled_basic = basic.len() as u32;
    if handled_basic > 0 {
        out.extend(basic.iter());
        out.push('-');
    }
    let mut n = INITIAL_N;
    let mut delta: u32 = 0;
    let mut bias = INITIAL_BIAS;
    let mut handled = handled_basic;
    let total = chars.len() as u32;
    while handled < total {
        let m = chars
            .iter()
            .map(|c| *c as u32)
            .filter(|cp| *cp >= n)
            .min()?;
        delta = delta.checked_add((m - n).checked_mul(handled + 1)?)?;
        n = m;
        for &ch in &chars {
            let cp = ch as u32;
            if cp < n {
                delta = delta.checked_add(1)?;
            }
            if cp == n {
                let mut q = delta;
                let mut k = BASE;
                loop {
                    let t = if k <= bias {
                        TMIN
                    } else if k >= bias + TMAX {
                        TMAX
                    } else {
                        k - bias
                    };
                    if q < t {
                        break;
                    }
                    out.push(digit(t + (q - t) % (BASE - t)));
                    q = (q - t) / (BASE - t);
                    k += BASE;
                }
                out.push(digit(q));
                bias = adapt(delta, handled + 1, handled == handled_basic);
                delta = 0;
                handled += 1;
            }
        }
        delta = delta.checked_add(1)?;
        n = n.checked_add(1)?;
    }
    Some(out)
}

/// Допустимая ASCII-метка домена.
fn valid_ascii_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 63
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Домен в ASCII-форме (`xn--…` для не-ASCII меток).
pub fn domain_to_ascii(domain: &str) -> Option<String> {
    let trimmed = domain.trim().trim_end_matches('.').to_lowercase();
    if trimmed.is_empty() || trimmed.len() > 253 {
        return None;
    }
    let mut out: Vec<String> = Vec::new();
    for label in trimmed.split('.') {
        if label.is_empty() {
            return None;
        }
        if label.is_ascii() {
            if !valid_ascii_label(label) {
                return None;
            }
            out.push(label.to_string());
            continue;
        }
        // уже в punycode — оставляем, проверив форму
        let encoded = if let Some(rest) = label.strip_prefix("xn--") {
            format!("xn--{rest}")
        } else {
            format!("xn--{}", encode_label(label)?)
        };
        if !valid_ascii_label(&encoded) {
            return None;
        }
        out.push(encoded);
    }
    let joined = out.join(".");
    if joined.is_empty() || joined.len() > 253 {
        return None;
    }
    Some(joined)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_examples() {
        // сверено с python: "прокупол.рф".encode("idna")
        assert_eq!(
            domain_to_ascii("прокупол.рф").as_deref(),
            Some("xn--j1achbcce0a.xn--p1ai")
        );
        assert_eq!(
            domain_to_ascii("пример.рф").as_deref(),
            Some("xn--e1afmkfd.xn--p1ai")
        );
        assert_eq!(
            domain_to_ascii("москва.рф").as_deref(),
            Some("xn--80adxhks.xn--p1ai")
        );
        assert_eq!(
            domain_to_ascii("test.пример.рф").as_deref(),
            Some("test.xn--e1afmkfd.xn--p1ai")
        );
    }

    /// Эталоны получены python: `"…".encode("idna")`. Покрывают разные ветки
    /// `adapt`: короткие метки, длинные, смешанные ASCII+кириллица, CJK, латиница
    /// с диакритикой. Добавлены после прогона `cargo mutants`: без них мутации
    /// в арифметике bias не ловились.
    ///
    /// Латинские буквы с диакритикой (`ü`, `ö`) сюда не входят осознанно:
    /// python-кодек `idna` делает Nameprep (NFKC + свёртка регистра), а мы
    /// нормализацию Unicode не выполняем — см. заголовок модуля.
    #[test]
    fn reference_vectors() {
        let cases = [
            (
                "оченьдлинныйдомендляпроверкиадаптации.рф",
                "xn--80aaafjaacdcc5afeaewkfrpdaenjf1aivd4e5ge1m4a9j.xn--p1ai",
            ),
            ("мама-мыла-раму.рф", "xn-----6kcabb8eebce0c2b7g.xn--p1ai"),
            ("test-пример.рф", "xn--test--3weu6azakd.xn--p1ai"),
            ("日本語.jp", "xn--wgv71a119e.jp"),
            (
                "aбвгдежзийклмнопрстуфхцчшщъыьэюя.рф",
                "xn--a-btbdefghijklmnopqrstuvwxyz0a1a2a3a4a5a6a.xn--p1ai",
            ),
            ("почта.рф", "xn--80a1acny.xn--p1ai"),
        ];
        for (input, want) in cases {
            assert_eq!(
                domain_to_ascii(input).as_deref(),
                Some(want),
                "вход: {input}"
            );
        }
    }

    /// Границы, которые мутационный анализ пометил как непокрытые:
    /// длина метки, длина домена и пустая метка.
    #[test]
    fn length_boundaries() {
        // метка из 63 символов допустима, из 64 — уже нет
        let ok_label = "a".repeat(63) + ".ru";
        assert_eq!(
            domain_to_ascii(&ok_label).as_deref(),
            Some(ok_label.as_str())
        );
        let long_label = "a".repeat(64) + ".ru";
        assert!(domain_to_ascii(&long_label).is_none());

        // домен ровно 253 символа проходит, 254 — нет
        let mut parts = Vec::new();
        let mut total = 0;
        while total + 64 <= 253 {
            parts.push("a".repeat(63));
            total += 64;
        }
        let tail_len = 253 - total;
        parts.push("b".repeat(tail_len));
        let domain = parts.join(".");
        assert_eq!(domain.len(), 253, "собранный домен должен быть ровно 253");
        assert_eq!(domain_to_ascii(&domain).as_deref(), Some(domain.as_str()));

        let too_long = format!("{domain}.c");
        assert!(domain_to_ascii(&too_long).is_none());

        // пустая метка и висящая точка
        assert!(domain_to_ascii("a..ru").is_none());
        assert_eq!(domain_to_ascii("rdbzh.ru.").as_deref(), Some("rdbzh.ru"));
    }

    /// Длинная CJK-метка заставляет цикл bias в `adapt` прокрутиться
    /// несколько раз — этой ветки не хватало после первого прогона mutants.
    #[test]
    fn long_cjk_label() {
        let label = "日本語のドメイン名はとても長くなることがあります";
        assert_eq!(
            domain_to_ascii(&format!("{label}.jp")).as_deref(),
            Some("xn--l8jtaiq6azendq0bl2qtb1hk70b2uqnnlh113dwk4ct4f682uso3b.jp")
        );
    }

    #[test]
    fn ascii_passes_through() {
        assert_eq!(
            domain_to_ascii("Example.COM").as_deref(),
            Some("example.com")
        );
        assert_eq!(domain_to_ascii("rdbzh.ru").as_deref(), Some("rdbzh.ru"));
        assert_eq!(
            domain_to_ascii("xn--e1afmkfd.xn--p1ai").as_deref(),
            Some("xn--e1afmkfd.xn--p1ai")
        );
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(domain_to_ascii("").is_none());
        assert!(domain_to_ascii("-bad.ru").is_none());
        assert!(domain_to_ascii("a..b").is_none());
        assert!(domain_to_ascii("плохая метка.рф").is_none());
    }
}
