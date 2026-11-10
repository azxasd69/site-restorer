# Сборки: что, где и сколько весит

Основная реализация — **Rust** (`src/`, нативное окно egui + CLI в одном бинарнике).
В репозитории также живут: исходная версия на Python (`legacy/python`) и
экспериментальный порт на Go (`ports/go`).

Все файлы прикладываются к каждому релизу тегом `v*` и доступны по адресу
`https://github.com/azxasd69/site-restorer/releases/latest/download/<имя файла>`.
В каждом релизе есть `SHA256SUMS.txt` — контрольные суммы всех артефактов.

## Что скачивать

| Файл | Платформа | Размер | Что внутри |
|---|---|---|---|
| `SiteRestorer.exe` | Windows 10/11 x64 | 6,2 МБ | **Rust + egui**, основной вариант: окно, RU/EN |
| `SiteRestorer-x86.exe` | Windows 10/11 x86 | 5,6 МБ | Rust + egui, 32-битная сборка |
| `SiteRestorer-rust-win-cli-x64.exe` | Windows x64 | 2,7 МБ | Rust, только командная строка |
| `SiteRestorer-rust-win-cli-x86.exe` | Windows x86 | 2,1 МБ | Rust, самый маленький бинарник для Windows |
| `SiteRestorer-rust-linux-cli` | Linux x64 | 3,4 МБ | Rust, статический ELF (`ldd` → not a dynamic executable) |
| `SiteRestorer-rust-linux-gui` | Linux x64 | 9,1 МБ | Rust + egui (нужны системные X11/GL) |
| `SiteRestorer-rust-linux-i586-cli` | Linux i586 | — | опциональная цель: собирается, если на раннере есть 32-битный TLS-тулчейн |
| `SiteRestorer-python-win-x64.exe` | Windows x64 | 12,6 МБ | классическая версия на Python + Tk |
| `SiteRestorer-python-win-x86.exe` | Windows x86 | 10,2 МБ | классическая версия, 32 бита |
| `SiteRestorer-python-linux-x64` | Linux x64 | 27 МБ | классическая версия (X11) |
| `siterestorer-go-linux-x64` / `-linux-arm64` | Linux | 8,0 / 7,6 МБ | Go, один файл без зависимостей |
| `siterestorer-go-win-x64.exe` / `-win-x86.exe` | Windows | 8,3 / 7,9 МБ | Go |

Размеры сняты с релиза **v3.0.0**. Python-сборка на Linux в CI получается крупнее
локальной (≈13,6 МБ): раннер кладёт в onefile больше библиотек Tcl/Tk.

## Чем отличаются реализации

| | Rust + egui (основная) | Python + Tk (классическая) | Go (эксперимент) |
|---|---|---|---|
| Интерфейс | нативное окно, RU/EN | нативное окно на Tk, RU/EN | CLI |
| Зависимости при запуске | нет (Windows — MSVC-рантайм системный; Linux CLI — статика) | нет (PyInstaller onefile) | нет |
| Разбор адреса (раскладка, IDN) | да | да | да |
| Инвентарь CDX, докачка, AIMD | да | да | да |
| Переписывание ссылок, кодировки | да | да | да |
| Отчёт `report.html` | да | да | да |
| Тесты | 19 (`cargo test`) | 17 (`unittest`) | 24 (`go test`) |
| Размер | 2,1–9,1 МБ | 10–27 МБ | 7,6–8,3 МБ |

Живые прогоны: Rust — `prokupol.ru` целиком (518 адресов, 515 файлов, AIMD 2→19
потоков), GUI до 100 %; Python — `example.com` 1/1; Go — `example.com`, полный
конвейер с JSON-артефактами.

## Как собрать самому

### Rust (основная реализация)

```bash
cargo build --release                                    # GUI + CLI
cargo build --release --no-default-features              # только CLI
cargo build --release --no-default-features --target x86_64-unknown-linux-musl   # статика
rustup target add i686-pc-windows-msvc x86_64-pc-windows-msvc
cargo build --release --target i686-pc-windows-msvc      # Windows x86
cargo build --release --target x86_64-pc-windows-msvc    # Windows x64
```

Linux GUI требует системных пакетов: `libxkbcommon-dev libgl1-mesa-dev
libegl1-mesa-dev libx11-dev libxcursor-dev libxrandr-dev libxi-dev
libwayland-dev libfontconfig1-dev`.

Профиль релиза: `opt-level = "z"`, LTO, `panic = "abort"`, strip.

### Python (классическая версия)

```powershell
cd legacy/python
py -3.14 -m venv .venv
.venv\Scripts\pip install pyinstaller==6.22.3
.\build.ps1              # dist\SiteRestorer.exe
.\build.ps1 -Arch x86    # 32-битная сборка
```

Linux:

```bash
cd legacy/python
python3 -m venv .venv && .venv/bin/pip install pyinstaller==6.22.3
.venv/bin/pyinstaller --onefile --name SiteRestorer src/main.py
```

### Go (эксперимент)

```bash
cd ports/go
CGO_ENABLED=0 GOOS=linux  GOARCH=amd64 go build -trimpath -ldflags "-s -w" -o dist/siterestorer-go-linux-x64 .
CGO_ENABLED=0 GOOS=linux  GOARCH=arm64 go build -trimpath -ldflags "-s -w" -o dist/siterestorer-go-linux-arm64 .
CGO_ENABLED=0 GOOS=windows GOARCH=amd64 go build -trimpath -ldflags "-s -w" -o dist/siterestorer-go-win-x64.exe .
CGO_ENABLED=0 GOOS=windows GOARCH=386   go build -trimpath -ldflags "-s -w" -o dist/siterestorer-go-win-x86.exe .
```

## Командная строка

```
site-restorer --cli --site <адрес> --out <папка> [--limit N] [--workers N]
site-restorer --version
```

- `--site` — любой ввод: URL, домен, ссылка из архива или поиска, адрес в русской
  раскладке, кириллический домен;
- `--out` — папка результата (`site/`, `mirror/`, `data/`, `report.html`);
- `--limit` — ограничить число адресов (быстрая проба);
- `--workers` — потолок параллельных загрузок (внутри работает AIMD);
- `--rate` — максимум запросов в секунду (общий для всех воркеров);
- `--lang` — язык окна (`ru`/`en`); сообщения CLI остаются русскими.

У Go-варианта дополнительно `--stage all|inventory|download|build|verify|report`
и `--sanitize "<строка>"` — только разобрать адрес и выйти.

## Что проверяется автоматически

На каждый push и pull request:

- Python: `compileall` + 17 юнит-тестов (`legacy/python`);
- Rust: 19 тестов (`cargo test`);
- Go: `go vet` + 24 теста (`ports/go`);
- сборка всех бинарников, smoke-проверка `--version` у Rust-CLI и PE-заголовков
  у Python-сборок.

На тег `v*` те же артефакты прикладываются к релизу вместе с `SHA256SUMS.txt`.

## Ограничения, общие для всех реализаций

- Восстанавливается только то, что архив сохранил: файлы, которые краулер не
  забирал, отсутствуют навсегда.
- Динамика CMS (корзина, вход, поиск, AJAX) в статической копии не работает.
- Internet Archive ограничивает скорость с одного IP: большие сайты качаются
  часами, ответы 429/503 — нормальная часть работы.
- Кэпы: 50 000 адресов и 200 страниц CDX на один прогон.
- 32-битная сборка под Linux (i586) — опциональная цель: `ring` (TLS) не
  собирается под `i686-unknown-linux-musl` без multilib-тулчейна, поэтому
  артефакт появляется только когда цель собирается на раннере.

### GUI и консоль

`SiteRestorer.exe` (GUI) собирается как приложение подсистемы windows — окно
консоли при запуске не появляется. Для командной строки используйте
`SiteRestorer-rust-win-cli-*.exe` (собирается с `--no-default-features`);
если запустить GUI-файл с `--cli`, он подключится к консоли терминала, а при
перенаправлении вывода в файл — будет писать в файл.
