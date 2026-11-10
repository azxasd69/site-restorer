//! Минимальный слой переводов для сообщений ядра и CLI.
//!
//! GUI берёт подписи из `i18n`, а ядро (`core`) пишет в журнал и консоль —
//! для него нужен всего один переключатель: язык выбирается один раз при
//! старте (`--lang` или `SR_LANG`) и действует на весь процесс.

use std::sync::atomic::{AtomicBool, Ordering};

static ENGLISH: AtomicBool = AtomicBool::new(false);

/// Переключить язык сообщений ядра.
pub fn set_english(on: bool) {
    ENGLISH.store(on, Ordering::Relaxed);
}

pub fn is_english() -> bool {
    ENGLISH.load(Ordering::Relaxed)
}

/// Выбрать строку по языку.
pub fn t(ru: &'static str, en: &'static str) -> &'static str {
    if is_english() { en } else { ru }
}

/// То же для заранее собранных строк (форматирование с аргументами).
pub fn pick(ru: String, en: String) -> String {
    if is_english() { en } else { ru }
}
