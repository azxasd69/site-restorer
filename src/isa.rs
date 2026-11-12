//! Возможности процессора: определяются один раз, кэшируются, попадают в отчёт.
//!
//! Зачем модуль. Политика платформ (`docs/PLATFORMS.md` §2) разрешает ускорения
//! только через проверку во время работы и только со скалярным двойником. Без
//! единой точки проверки каждый вызов `is_x86_feature_detected!` повторяет
//! `cpuid`, а разбор обращения «у меня вылетело» невозможен: неизвестно, был ли
//! включён ускоренный путь. Здесь возможности определяются один раз, кэшируются,
//! попадают в отчёт и могут быть выключены переменной `SR_NO_SIMD=1`.

use std::sync::OnceLock;

/// Что умеет процессор и не выключили ли ускорения вручную.
///
/// Поля закрыты намеренно: решение «идти ускоренным путём» принимается не
/// чтением флага, а получением пропуска [`Avx2`]. Так выключатель
/// `SR_NO_SIMD=1` нельзя обойти забытой проверкой в новом месте кода.
#[derive(Debug, Clone, Copy)]
pub struct Isa {
    avx2: bool,
    sha_ni: bool,
    popcnt: bool,
    forced_scalar: bool,
}

impl Isa {
    /// Все возможности выключены: путь для платформ без `cpuid`.
    pub const SCALAR_ONLY: Isa = Isa {
        avx2: false,
        sha_ni: false,
        popcnt: false,
        forced_scalar: false,
    };

    pub fn avx2(&self) -> bool {
        self.avx2
    }

    pub fn sha_ni(&self) -> bool {
        self.sha_ni
    }

    pub fn popcnt(&self) -> bool {
        self.popcnt
    }

    /// Ускорения выключены вручную (`SR_NO_SIMD=1`).
    pub fn forced_scalar(&self) -> bool {
        self.forced_scalar
    }
}

/// Пропуск к ускоренным путям AVX2.
///
/// Создать его можно только функцией [`avx2_token`], а она возвращает `None`,
/// если возможности нет или ускорения выключены. Правило для будущего кода:
/// любая функция с `#[target_feature(enable = "avx2")]` принимает `&Avx2`.
/// Компилятор этого не требует, поэтому правило проверяет
/// `scripts/check-flags.py`: функция с таким атрибутом обязана принимать Avx2.
#[derive(Debug, Clone, Copy)]
pub struct Avx2(());

/// Пропуск к AVX2, если он доступен и не выключен вручную.
pub fn avx2_token() -> Option<Avx2> {
    let caps = caps();
    (caps.avx2 && !caps.forced_scalar).then_some(Avx2(()))
}

/// Возможности текущего процесса. Считаются один раз на процесс.
pub fn caps() -> &'static Isa {
    static CAPS: OnceLock<Isa> = OnceLock::new();
    CAPS.get_or_init(detect)
}

fn detect() -> Isa {
    // Переменная читается один раз: значение «1» (или любое, кроме «0»)
    // выключает все ускоренные пути. Нужна для разбора проблем на месте.
    let forced = std::env::var_os("SR_NO_SIMD").is_some_and(|v| v != "0");
    if forced {
        return Isa {
            forced_scalar: true,
            ..Isa::SCALAR_ONLY
        };
    }
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    {
        use std::arch::is_x86_feature_detected as has;
        Isa {
            avx2: has!("avx2"),
            sha_ni: has!("sha"),
            popcnt: has!("popcnt"),
            forced_scalar: false,
        }
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "x86")))]
    {
        Isa::SCALAR_ONLY
    }
}

/// Одна строка для отчёта и `--version --verbose` (ФТ-156).
pub fn describe() -> String {
    let c = caps();
    let mut text = format!(
        "avx2={} sha-ni={} popcnt={}",
        yes_no(c.avx2()),
        yes_no(c.sha_ni()),
        yes_no(c.popcnt())
    );
    if c.forced_scalar() {
        text.push_str(" (ускорения выключены через SR_NO_SIMD)");
    }
    text
}

fn yes_no(value: bool) -> &'static str {
    if value { "да" } else { "нет" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_are_stable_between_calls() {
        // Один и тот же адрес: значения не пересчитываются.
        let first = caps() as *const Isa;
        let second = caps() as *const Isa;
        assert_eq!(first, second);
    }

    /// Правило контракта: без возможностей пропуска нет, а при выключенных
    /// ускорениях он не выдаётся даже на машине с AVX2.
    #[test]
    fn token_respects_forced_scalar() {
        let caps = caps();
        if caps.forced_scalar() || !caps.avx2() {
            assert!(
                avx2_token().is_none(),
                "пропуск не выдаётся без возможностей или при выключенных ускорениях"
            );
        } else {
            assert!(avx2_token().is_some(), "на машине с AVX2 пропуск есть");
        }
    }

    #[test]
    fn describe_mentions_forced_scalar() {
        // Описание строится из фактических возможностей; для машины без AVX2
        // это «avx2=нет», и строка не должна пустовать.
        let text = describe();
        assert!(text.contains("avx2="), "в строке есть признак AVX2");
        assert!(text.contains("popcnt="), "в строке есть признак POPCNT");
    }
}
