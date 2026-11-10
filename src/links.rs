//! Внешние ссылки: адреса и открытие браузером по умолчанию.
//!
//! Адреса заданы здесь в одном месте: их нельзя набирать в разметке окна,
//! иначе они разойдутся с требованиями ФТ-114 и ФТ-115.

use crate::i18n::Lang;

/// Сайт продукта.
pub const SITE: &str = "https://lastcopy.ru/";
/// Страница поддержки автора.
pub const SUPPORT: &str = "https://boosty.to/explosion/donate";

/// Заголовок окна: имя, версия сборки и подзаголовок выбранного языка.
///
/// Версия подставляется из `Cargo.toml`, поэтому при выпуске новой версии
/// заголовок меняется сам и не расходится с `--version`.
pub fn window_title(lang: Lang) -> String {
    format!("Site Restorer v{} — {}", crate::APP_VERSION, lang.t().title)
}

/// Открывает адрес в браузере по умолчанию.
///
/// Ошибку возвращаем текстом: её пишут в журнал, окно при этом продолжает
/// работать — прогон не прерывается и прогресс не сбрасывается.
pub fn open(url: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let spawned = std::process::Command::new("rundll32")
        .arg("url.dll,FileProtocolHandler")
        .arg(url)
        .spawn();
    #[cfg(not(target_os = "windows"))]
    let spawned = std::process::Command::new("xdg-open").arg(url).spawn();
    spawned.map(|_| ()).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_exact() {
        assert_eq!(SITE, "https://lastcopy.ru/");
        assert_eq!(SUPPORT, "https://boosty.to/explosion/donate");
    }

    #[test]
    fn addresses_are_plain_https_without_tracking() {
        for url in [SITE, SUPPORT] {
            assert!(url.starts_with("https://"), "{url}");
            assert!(!url.contains('?'), "{url}");
            assert!(!url.contains("utm_"), "{url}");
        }
    }

    #[test]
    fn title_has_product_name_and_version() {
        for lang in [Lang::Ru, Lang::En] {
            let title = window_title(lang);
            assert!(title.starts_with("Site Restorer v"), "{title}");
            assert!(title.contains(crate::APP_VERSION), "{title}");
            assert_eq!(title.matches('—').count(), 1, "{title}");
        }
    }
}
