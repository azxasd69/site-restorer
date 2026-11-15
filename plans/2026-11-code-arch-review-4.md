# Архитектурное и код-ревью изменений релиза 3.6.6

| Поле | Значение |
|---|---|
| Статус | завершено, замечания устранены |
| Версия | 1.0 |
| Дата | 15.11.2026 |
| Основание | запрос владельца: выполнить разработку `3.6.6` (оптимизированный алгоритм, `Д-49`, замечания `plans/2026-11-proverka-3-sajtov-kopii.md`), сделать код-ревью и выпустить релиз |
| Область | `D:\dev\sr-docs` (`src/core.rs`, `src/serve.rs`, `src/encoding.rs`, `src/main.rs`, `src/app.rs`, `src/i18n.rs`), сравнение `v3.6.5..HEAD` |

---

## 1. Вывод ревью

- **Архитектура и границы модулей:** встроенный сервер выделен в автономный модуль `src/serve.rs` (`495` строк кода до тестов, **0** новых крейтов в `Cargo.toml`/`Cargo.lock`, **0** `unsafe`), переиспользующий уже существующие правила маршрутизации `paths::url_to_local_path`, декодер `sanitize::pct_decode` и `core::long_path`.
- **Безопасность путей и файловых операций:** автоматический скан `fs::*` в `src/core.rs` выявил `0` вызовов без `long_path`. В ходе ревью `src/serve.rs` обнаружен и сразу исправлен 1 пропуск `long_path` в `check_candidate` (`fs::canonicalize(long_path(&candidate))`).
- **Логика конвейера (обнаруженный и устранённый дефект инкрементальной сборки):** при ревью `drop_orphans()` (`src/core.rs:4512`) выявлено, что при полном прогоне (`limit == 0`, повторный запуск) файлы бандлов минификатора `site/_min/<fnv>.css` удалялись как «отсутствующие в `mirror/`». Добавлен фильтр `!rel.starts_with("_min/")`.
- **Качество кода и статический анализ:** в `src/serve.rs` `0` `unsafe`, в `src/encoding.rs` `0` `unsafe` (исправлен `clippy::collapsible_if` в `repair_mixed_utf8_in_cp1251`), `unsafe` в `src/main.rs` ограничен системными вызовами Win32 (`AttachConsole`, `SetConsoleOutputCP`, `MessageBoxW`, `OleInitialize`, `SetConsoleCtrlHandler`) и POSIX (`libc::signal` для `--serve`).

---

## 2. Проверка изменений по блокам

| Модуль / блок | Что проверено | Находки и меры |
|---|---|---|
| `src/core.rs` (`Gate`, `GateSlot`, `aimd_step`, `ureq` пул) | Блокировки, освобождение слота при `sleep()`, согласование `cap = workers.clamp(1, 24)`, потоковый `win_bytes` в `save_body()`, отсутствие двойного счёта | Убран неиспользуемый `size` из `Attempt::Saved` (чистый `clippy`), проверены тесты `gate_respects_worker_cap`, `aimd_handles_small_files_streak_and_overload` |
| `src/core.rs` (`inventory`, `rescue`, `cleanup_temp`, `SaveErr`) | Разделение ошибок `SaveErr::Io` (без повтора) vs `SaveErr::Stream` (повтор), нормализация `\` в `long_path`, пути `rescue` через `url_to_local_path_in`, реальная уборка `.part` | Выявлено при живом тесте `exp366-default150` (`os error 3` в `rescue` из-за прямых `/` в `\\?\` и ключе каталога без `/index.html`) — устранено в `long_path` и `rescue()`, подтверждено тестом `rescue_maps_directory_keys_to_index_html` |
| `src/core.rs` (`commit`, `build_dirty`, `rewrite_ref`, `limit_order`) | Магия `1f 8b` (`Д-138`), HTML под видом ресурса (`Д-140`), корневые ссылки (`Д-139`), стратификация (`Д-141`), поддомены (`Д-142`), минификатор (`Д-143`) | Выявлен `drop_orphans()` на `_min/` → добавлено исключение `!rel.starts_with("_min/")`. Проверено, что в `snabmetiz-366` 15 сжатых файлов распакованы (`2 099` правил CSS в Chrome) |
| `src/serve.rs` (`Д-49`) | `127.0.0.1`, `GET`/`HEAD`, `Range` `206`/`416`, `ETag`/`304`, traversal (`..`, `%2e%2e`, `\`, `\0`), пул 4 воркера, остановка по `stop()`/`Drop` | Добавлен `long_path` в `check_candidate`; в `serve::tests` исправлена проверка последней строки заголовков; тесты `serves_index_query_params_and_mime_without_charset` и `enforces_methods_traversal_range_head_and_etag` зелёные |
| `src/encoding.rs` (`Д-145`) | Починка `windows-1251` + UTF-8 (`e2 80 93`, кириллица), защита легального `С«текст»` | Исправлен `clippy::collapsible_if`; тест `mixed_cp1251_with_utf8_dash_is_repaired` зелёный |
| `src/main.rs`, `src/app.rs`, `src/i18n.rs` | Флаги `--serve`, `--serve --out`, `--port`, `--include-host`, кнопка «Открыть копию» / «Остановить сервер», остановка при `start()` и `on_exit()` | Проверены разбор и валидация флагов (`serve_flags_are_parsed_and_validated`), GUI и CLI `clippy -D warnings` чистые |

---

## 3. Результаты локальных гейтов

| Гейт | Результат |
|---|---|
| `cargo fmt --all -- --check` | код `0` |
| `cargo clippy --all-targets --all-features -- -D warnings` | код `0` (`0` предупреждений) |
| `cargo clippy --all-targets --no-default-features -- -D warnings` | код `0` (`0` предупреждений) |
| `cargo nextest run --all-features` | **`150/150` passed** (`1` skipped `bench_parse_rows`) |
| `cargo nextest run --no-default-features` | **`147/147` passed** (`1` skipped `bench_parse_rows`) |
| `python scripts/check-flags.py` | код `0` (`3` цели, единственный источник `.cargo/config.toml`) |
| `cargo audit` | `0` уязвимостей (1509 advisories) |
| `cargo deny check` | `advisories ok, bans ok, licenses ok, sources ok` |
| `cargo machete` | `0` неиспользуемых зависимостей |
| `typos` | `0` опечаток |

## 4. Что не проверено в рамках локального ревью

- Кросс-компиляция Linux GUI / Linux musl CLI (`x86_64-unknown-linux-gnu` / `musl`) на Windows не запускалась — проверяется конвейером GitHub Actions на теге `v3.6.6`.
- Интерактивный клик кнопки «Открыть копию» в окне GUI вручную не выполнялся (проверены логика `CopyServer::start`/`stop`, `clippy` GUI-сборки, 150 тестов и CLI `--serve` в браузере Chrome).
