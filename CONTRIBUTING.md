# Contributing

Спасибо, что хотите помочь. Ниже — короткий и обязательный минимум.

## Окружение

Разработка ведётся только на Rust. Python- и Go-реализации переведены в архив
(`legacy/python`, `ports/go`): они не собираются, не тестируются и не попадают
в релиз. Правки в них не принимаются.

- Windows 10/11 x64 (для 32-битной сборки — MSVC x86), либо Linux x64.
- Rust — версия закреплена в `rust-toolchain.toml` (компоненты `clippy` и `rustfmt`
  подтянутся сами).
- Для Windows-сборок — MSVC Build Tools; для Linux-GUI — `libxkbcommon-dev`,
  `libgl1-mesa-dev`, `libegl1-mesa-dev`.

```powershell
git clone https://github.com/azxasd69/site-restorer.git
cd site-restorer
cargo build --release            # GUI: target\release\site-restorer.exe
cargo build --release --no-default-features   # CLI
```

## Тесты

```powershell
cargo nextest run --release --no-default-features
```

Тесты не ходят в сеть: проверяются парсер адреса, построение путей, punycode,
разбор архива. CI запускает их на каждый push и pull request — PR без зелёных
тестов не мержится.

## Проверки перед PR

```powershell
cargo fmt --all --check
cargo clippy --all-targets --release --no-default-features -- -D warnings
cargo clippy --all-targets --release -- -D warnings   # GUI
cargo nextest run --release --no-default-features
cargo deny check
cargo machete
typos
```

Всё это же делает CI (`.github/workflows/build.yml`), поэтому локальный прогон
экономит цикл.

## Сборка и релиз

Матрица сборок, лимиты размера и порядок выпуска — в [BUILD.md](BUILD.md) и
[docs/RELEASE.md](docs/RELEASE.md). Релиз делает CI по тегу; вручную бинарники
в релиз не заливаются.

## Правила кода

- Новые зависимости — отдельное обсуждение в issue: размер бинарника растёт
  от каждой (см. `cargo bloat`).
- Публичные функции — с docstring; сложные места — с комментарием «почему», а не «что».
- Никакой маскировки под браузер, обхода капчи и ротации IP. Инструмент работает
  с архивом вежливо: честный User-Agent, Retry-After, паузы.
- Пути и имена файлов — только через `url_to_local_path()`; она гарантирует
  детерминизм, лимит длины и запрещённые имена Windows.

## Коммиты

Формат: `<область>: <краткое описание>` — например `ui: show live byte counters`
или `core: deterministic hashing for file names`. Один коммит — одна мысль.
Сообщения — на английском.

## Pull request

1. Ответвление от `main`, ветка `fix/...` или `feat/...`.
2. Тесты и проверки выше проходят, сборка собирается.
3. В описании: что меняется, зачем, как проверено (команды и результат).
4. Изменения поведения UI — со скриншотом до/после.

## Что приветствуется

- Поддержка новых CMS-эвристик (с примером сайта и объяснением).
- Улучшение диагностики и отчётов.
- Переводы интерфейса.
- Тесты на найденные баги (issue с воспроизведением — идеально).
