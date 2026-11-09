# Сборки: что, где и сколько весит

Проект собирается на трёх стеках. Все бинарники прикладываются к каждому релизу
тегом `v*` и доступны по адресу
`https://github.com/azxasd69/site-restorer/releases/latest/download/<имя файла>`.

## Что скачивать

| Файл | Платформа | Размер | Стек | Когда брать |
|---|---|---|---|---|
| `SiteRestorer.exe` | Windows 10/11 x64 | 13,0 МБ | Python + Tk | Основной вариант: привычный интерфейс, RU/EN |
| `SiteRestorer-x86.exe` | Windows 10/11 x86 | 10,5 МБ | Python + Tk | 32-битные системы |
| `SiteRestorer-linux` | Linux x64 | 13,6 МБ | Python + Tk | Знакомый интерфейс на Linux (нужен X11) |
| `SiteRestorer-rust-win-gui-x64.exe` | Windows x64 | 6,9 МБ | Rust + egui | Тот же функционал, вдвое меньше и быстрее старт |
| `SiteRestorer-rust-win-gui-x86.exe` | Windows x86 | 6,2 МБ | Rust + egui | 32-битные системы, минимальный GUI |
| `SiteRestorer-rust-win-cli-x64.exe` | Windows x64 | 2,6 МБ | Rust | Только командная строка, без GUI |
| `SiteRestorer-rust-win-cli-x86.exe` | Windows x86 | 2,0 МБ | Rust | Самый маленький бинарник для Windows |
| `SiteRestorer-rust-linux-cli` | Linux x64 | 3,3 МБ | Rust | Статический ELF: `ldd` → not a dynamic executable |
| `SiteRestorer-rust-linux-gui` | Linux x64 | 8,6 МБ | Rust + egui | Нативное окно на Linux (нужны X11/GL) |
| `siterestorer-go-linux-x64` | Linux x64 | 8,5 МБ | Go | Один файл, без зависимостей |
| `siterestorer-go-linux-arm64` | Linux arm64 | 7,9 МБ | Go | ARM-серверы и Raspberry Pi |
| `siterestorer-go-win-x64.exe` | Windows x64 | 8,8 МБ | Go | Альтернативная реализация |
| `siterestorer-go-win-x86.exe` | Windows x86 | 8,5 МБ | Go | Альтернативная реализация, 32 бита |

Размеры — замеры на релизной сборке (`--release`, LTO, strip; для Go —
`CGO_ENABLED=0 -trimpath -ldflags "-s -w"`). Точные значения для конкретного
тега видны на странице релиза.

## Чем отличаются реализации

| | Python + Tk | Rust + egui | Go |
|---|---|---|---|
| Интерфейс | Tk, тёмный журнал | egui, светлая тема | CLI (GUI не делали) |
| Зависимости при запуске | нет (PyInstaller onefile) | GUI — системные X11/GL; CLI — нет | нет |
| Разбор адреса (раскладка, IDN) | да | да | да |
| Инвентарь CDX, докачка, AIMD | да | да | да |
| Переписывание ссылок, кодировки | да | да | да |
| Отчёт `report.html` | да | да | да |
| Тесты | 17 (unittest) | 19 (cargo test) | 24 (go test) |
| Размер | 10–14 МБ | 2,0–8,6 МБ | 7,9–8,8 МБ |

Проверено живыми прогонами: все три реализации скачивают `example.com` (1 из 1,
отчёт создаётся), Rust дополнительно прошёл `prokupol.ru --limit 25`
(24 из 25 файлов, 4 страницы, 345 внутренних ссылок) и полный прогон с GUI
до 100 %.

## Как собрать самому

### Python (Windows и Linux)

```powershell
py -3.14 -m venv .venv
.venv\Scripts\pip install pyinstaller==6.22.3
.\build.ps1              # dist\SiteRestorer.exe
.\build.ps1 -Arch x86    # dist\SiteRestorer-x86.exe
```

Linux:

```bash
python3 -m venv .venv && .venv/bin/pip install pyinstaller==6.22.3
.venv/bin/pyinstaller --onefile --name SiteRestorer src/main.py
```

### Rust (`ports/rust`)

```bash
# CLI, статический musl
cargo build --release --no-default-features --target x86_64-unknown-linux-musl
# GUI на Linux (нужны libxkbcommon-dev, libgl1-mesa-dev, libx11-dev, libxrandr-dev, libxcursor-dev, libxi-dev, libwayland-dev)
cargo build --release
# Windows: msvc-цели, GUI и CLI
cargo build --release --target x86_64-pc-windows-msvc
cargo build --release --no-default-features --target i686-pc-windows-msvc
```

Сборка под Windows требует MSVC (Build Tools 2022) и целей
`x86_64-pc-windows-msvc` / `i686-pc-windows-msvc`.

### Go (`ports/go`)

```bash
cd ports/go
CGO_ENABLED=0 GOOS=linux  GOARCH=amd64 go build -trimpath -ldflags "-s -w" -o dist/siterestorer-go-linux-x64 .
CGO_ENABLED=0 GOOS=linux  GOARCH=arm64 go build -trimpath -ldflags "-s -w" -o dist/siterestorer-go-linux-arm64 .
CGO_ENABLED=0 GOOS=windows GOARCH=amd64 go build -trimpath -ldflags "-s -w" -o dist/siterestorer-go-win-x64.exe .
CGO_ENABLED=0 GOOS=windows GOARCH=386   go build -trimpath -ldflags "-s -w" -o dist/siterestorer-go-win-x86.exe .
```

## Командная строка

Все реализации понимают одинаковый минимум:

```
site-restorer --cli --site <адрес> --out <папка> [--limit N] [--workers N]
```

- `--site` — любой ввод: URL, домен, ссылка из архива или поиска, адрес в русской раскладке, кириллический домен;
- `--out` — папка результата (`site/`, `mirror/`, `data/`, `report.html`);
- `--limit` — ограничить число адресов (быстрая проба);
- `--workers` — потолок параллельных загрузок (адаптивность AIMD работает внутри).

У Go-варианта дополнительно есть `--stage all|inventory|download|build|verify|report`
и `--sanitize "<строка>"` — только разобрать адрес и выйти.

## Что проверяется автоматически

На каждый push и pull request:

- `python -m compileall` и 17 юнит-тестов Python;
- `cargo test` для Rust (19 тестов: разбор адреса, пути, парсер CDX, кодировки, переписывание ссылок);
- `go vet` и `go test ./...` для Go (24 теста);
- сборка всех бинарников и проверка PE-заголовков у Windows-артефактов.

На тег `v*` те же сборки прикладываются к релизу.

## Ограничения, общие для всех реализаций

- Восстанавливается только то, что архив сохранил: файлы, которые краулер не забирал, отсутствуют навсегда.
- Динамика CMS (корзина, вход, поиск, AJAX) в статической копии не работает.
- Internet Archive ограничивает скорость с одного IP: большие сайты качаются часами, ответы 429/503 — нормальная часть работы.
- Кэпы: 50 000 адресов и 200 страниц CDX на один прогон.
