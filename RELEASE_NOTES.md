## 3.1.3 — 2026-11-11

Релизный процесс приведён к одной схеме; разработка только на Rust.

**Осталась одна реализация.** Python (`legacy/python`) и Go (`ports/go`) переведены
в архив: их джобы убраны из CI, сборки больше не попадают в релиз, в каталогах
и README стоит пометка «архив». Код оставлен как история.

**Имена файлов.** Единая схема `SiteRestorer-<os>-<arch>[-cli][.exe]`, без версии
в имени — ссылки `releases/latest/download/<имя>` постоянны:

| Было | Стало |
|---|---|
| `SiteRestorer.exe` | `SiteRestorer-windows-x64.exe` |
| `SiteRestorer-x86.exe` | `SiteRestorer-windows-x86.exe` |
| `SiteRestorer-rust-win-cli-x64.exe` | `SiteRestorer-windows-x64-cli.exe` |
| `SiteRestorer-rust-win-cli-x86.exe` | `SiteRestorer-windows-x86-cli.exe` |
| `SiteRestorer-rust-linux-cli` | `SiteRestorer-linux-x64-cli` |
| `SiteRestorer-rust-linux-gui` | `SiteRestorer-linux-x64-gui` |
| `site-restorer-linux-x64.tar.gz` | `SiteRestorer-linux-x64.tar.gz` |

**Гейты релиза:** тег обязан совпадать с версией в `Cargo.toml`; у каждого
бинарника проверяются `--version` и лимит размера; в релизе 8 файлов вместо 15.
Релиз-ноты берутся из этого файла, а не из списка коммитов.

**Тулчейн закреплён** (`rust-toolchain.toml`, 1.99.1) — и локально, и в CI,
чтобы сборка не зависела от того, какой stable попал на раннер.

**Документация:** [docs/RELEASE.md](docs/RELEASE.md) — что выпускаем, как называем,
как нумеруем, как собираем, что делать при ошибке. README и BUILD.md обновлены.

Сборка: Rust. Контрольные суммы — в SHA256SUMS.txt.
