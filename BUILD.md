# Сборки: что, где и сколько весит

Реализация одна — **Rust** (`src/`, нативное окно egui + CLI в одном исходнике).
Python (`legacy/python`) и Go (`ports/go`) переведены в архив: не развиваются,
не собираются в CI и не попадают в релизы.

Все файлы прикладываются к каждому релизу тегом `v*` и доступны по адресу
`https://github.com/azxasd69/site-restorer/releases/latest/download/<имя файла>`.
В каждом релизе есть `SHA256SUMS.txt` — контрольные суммы всех артефактов.

## Что скачивать

| Файл | Платформа | Размер | Что внутри |
|---|---|---|---|
| `SiteRestorer-windows-x64.exe` | Windows 10/11 x64 | 6,2 МБ | **основной вариант**: окно egui, RU/EN, статический CRT |
| `SiteRestorer-windows-x86.exe` | Windows 10/11 x86 | 5,6 МБ | то же, 32 бита |
| `SiteRestorer-windows-x64-cli.exe` | Windows x64 | 1,8 МБ | только командная строка |
| `SiteRestorer-windows-x86-cli.exe` | Windows x86 | 1,3 МБ | самый маленький бинарник для Windows |
| `SiteRestorer-linux-x64-gui` | Linux x64 | 7,5 МБ | окно egui (нужны системные X11/GL) |
| `SiteRestorer-linux-x64-cli` | Linux x64 | 2,1 МБ | статический ELF (`ldd` → not a dynamic executable) |
| `SiteRestorer-linux-x64.tar.gz` | Linux x64 | 4,5 МБ | оба бинарника + `.desktop`, иконка, `install.sh` |

Размеры сняты с релиза **v3.1.2**. Имя не содержит версию — ссылки
`releases/latest/download/<имя>` постоянны; версию печатает `--version`
и первая строка журнала окна.

## Что было раньше

| | Rust + egui (единственная) | Python + Tk (архив) | Go (архив) |
|---|---|---|---|
| Интерфейс | нативное окно, RU/EN | нативное окно на Tk | CLI |
| Статус | развивается, публикуется | заморожен, не публикуется | заморожен, не публикуется |
| Тесты | 50 (`cargo nextest`) | 17 (не запускаются в CI) | 24 (не запускаются в CI) |

Каталоги `legacy/python` и `ports/go` оставлены как история: код читается,
но сборки из них больше не выпускаются.

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
Версия тулчейна закреплена в `rust-toolchain.toml` (1.99.1) и используется в CI.

### Иконки

Иконки — знак с сайта продукта (lastcopy.ru), — но хранятся готовыми в `packaging/icons/`:
SVG-мастер, PNG 16…512, ICO и сырой RGBA для окна. Пересобрать — одной командой:

```bash
pip install pillow
python scripts/make-icons.py
```

Скрипт перезаписывает `packaging/icons/`; после этого `install -m644` для Linux — вручную
(строка в `packaging/linux/install.sh` уже есть). На Windows `build.rs` сам берёт
`packaging/icons/site-restorer.ico` и `packaging/windows/site-restorer.rc.in` и вызывает `rc.exe`
из Windows SDK: если SDK нет, сборка идёт без иконки и с предупреждением. Компилятор ресурсов
не понимает кириллицу в пути, поэтому `build.rs` передаёт ему короткие (8.3) пути.

### Python и Go

Не собираются. Каталоги `legacy/python` и `ports/go` — архив исходников;
в CI нет ни одной джобы, которая их трогает.

## Командная строка

```
site-restorer --cli --site <адрес> --out <папка> [--limit N] [--workers N] [--threads N]
                [--snapshot newest|oldest|<дата>] [--as-of ГГГГ[-ММ[-ДД]]] [--since ГГГГ[-ММ]]
                [--refresh-days N] [--refresh] [--fallback-depth N] [--rescue-limit N]
site-restorer --version
```

- `--site` — любой ввод: URL, домен, ссылка из архива или поиска, адрес в русской
  раскладке, кириллический домен;
- `--out` — папка результата (`site/`, `mirror/`, `data/`, `report.html`);
- `--limit` — ограничить число адресов (быстрая проба);
- `--workers` — потолок параллельных загрузок (внутри работает AIMD);
- `--threads` — потоков сборки копии и проверки ссылок (0 — по числу ядер, не больше 32);
- `--rate` — максимум запросов в секунду (общий для всех воркеров);
- `--snapshot` / `--as-of` — выбор снимка: самый свежий (по умолчанию), самый ранний или на дату;
- `--fallback-depth` — глубина добора к более старым снимкам (5);
- `--refresh-days` / `--refresh` — срок свежести файла и принудительная проверка;
- `--since` — окно адресов для страниц; `--rescue-limit` и `--rescue-budget` — добор по ссылкам и его бюджет времени;
- `--retry-missing` — срок повтора адресов, у которых не было снимка;
- `--lang` — язык окна (`ru`/`en`); сообщения CLI остаются русскими.

У архивного Go-варианта были свои `--stage` и `--sanitize`; в актуальной
реализации их нет.

## Что проверяется автоматически

На каждый push и pull request:

- `cargo fmt --all --check`;
- `cargo clippy --all-targets --release --no-default-features -- -D warnings`;
- `cargo nextest run --release --no-default-features` — 50 тестов;
- `cargo audit` (RUSTSEC), `cargo deny check` (лицензии и источники, политика
  в `deny.toml`), `cargo machete` (неиспользуемые зависимости), `typos`.

На тег `v*` дополнительно:

- гейт «тег = версия в `Cargo.toml`» (иначе сборка падает);
- сборка шести бинарников, у каждого — `--version` и лимит размера;
- `SiteRestorer-linux-x64.tar.gz`, `SHA256SUMS.txt`, релиз-ноты из `CHANGELOG.md`.

Те же команды локально — в разделе выше.

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

`SiteRestorer-windows-x64.exe` и `-windows-x86.exe` (GUI) собираются как
приложения подсистемы windows — окно консоли при запуске не появляется.
Для командной строки есть `SiteRestorer-windows-*-cli.exe` (сборка
с `--no-default-features`); если запустить GUI-файл с `--cli`, он подключится
к консоли терминала, а при перенаправлении вывода в файл — будет писать в файл.
