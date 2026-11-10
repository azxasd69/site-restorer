# Как опубликовать этот репозиторий на GitHub

Репозиторий уже инициализирован локально (`git init`, ветка `main`, первый коммит).
Осталось создать его на GitHub и отправить.

Аккаунт: **https://github.com/azxasd69**

---

## Почему репозиторий создаётся «пустым»

Галочки «Add a README / .gitignore / license» — это генератор стартовых файлов
для **пустого** репозитория. У нас они уже написаны и закоммичены. Если отметить
галочки, GitHub создаст свой первый коммит, истории окажутся несвязанными и
`git push` будет отклонён (`fetch first`). Придётся делать
`git pull --allow-unrelated-histories` и вручную разрешать конфликты в трёх файлах.

Правильный порядок для существующего проекта: **создать пустой репозиторий →
push готовой истории**. Именно так публикуют большинство open-source проектов.

## Шаг 1. Создать репозиторий

1. Откройте https://github.com/new
2. **Repository name:** `site-restorer`
3. **Description:** `Portable Windows tool that rebuilds a website from the Wayback Machine`
4. **Public**
5. **Не** ставьте галочки «Add a README», «Add .gitignore», «Choose a license» —
   они уже есть в этом репозитории.
6. **Create repository**

---

## Шаг 2. Настроить доступ (выберите один вариант)

### Вариант A — SSH-ключ (рекомендую, если пользуетесь этим ПК постоянно)

Ваш публичный ключ (скопируйте целиком):

```
ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEUlNNlPDUrmwQ2l6nsq5bxcw1YA3I7pvKh+q27UTrKL
```

1. https://github.com/settings/keys → **New SSH key**
2. Title: `Komp`, Key type: `Authentication key`, вставьте ключ → **Add SSH key**
3. Проверка:
   ```powershell
   ssh -T git@github.com
   ```
   Должно ответить `Hi azxasd69! You've successfully authenticated...`

### Вариант B — HTTPS + токен (если SSH не подходит)

1. https://github.com/settings/tokens → **Generate new token (classic)**
2. Scopes: `repo` (или fine-grained: Contents = Read and write)
3. Скопируйте токен — он показывается один раз.
4. При первом `git push` введите:
   - username: `azxasd69`
   - password: **токен** (не пароль от аккаунта)

---

## Шаг 3. Отправить код

Из папки репозитория:

```powershell
git remote add origin git@github.com:azxasd69/site-restorer.git   # вариант A
# или
git remote add origin https://github.com/azxasd69/site-restorer.git  # вариант B

git push -u origin main
```

Или одной командой:

```powershell
.\publish.ps1              # SSH
.\publish.ps1 -Https       # HTTPS
```

---

## Шаг 4. Выпустить релиз с exe

CI (`.github/workflows/build.yml`) собирает обе архитектуры и прикрепляет файлы
к релизу при пуше тега:

```powershell
git tag v2.4
git push origin v2.4
```

Через 3–5 минут на странице **Releases** появятся:
`SiteRestorer-windows-x64.exe` (x64) и `SiteRestorer-windows-x86.exe` (32-бит) — их и будут скачивать.

Если CI не нужен, загрузите exe вручную: **Releases → Draft a new release →**
тег `v2.4` → прикрепите файлы.

---

## Что уже готово в репозитории

| Файл | Назначение |
|---|---|
| `README.md` | описание, скриншоты, установка, CLI, ограничения |
| `LICENSE` | MIT |
| `CHANGELOG.md` | история версий |
| `ARCHITECTURE.md` | разбор алгоритмов |
| `src/` | исходники (`core.py`, `main.py`, `sanitize.py`, иконка) |
| `build.ps1` | локальная сборка x64/x86 |
| `.github/workflows/build.yml` | CI: сборка и релиз по тегу |
| `docs/screenshots/` | скриншоты для README |
| `docs/prokupol-case.md` | кейс восстановления prokupol.ru |
| `tests/` | юнит-тесты, которые гоняет CI |
| `CONTRIBUTING.md`, `SECURITY.md`, `CODE_OF_CONDUCT.md` | правила участия |
| `.github/ISSUE_TEMPLATE/`, `pull_request_template.md` | шаблоны обращений |

---

## Мелочи, которые стоит сделать после публикации

- **Topics** (шестерёнка рядом с About): `wayback-machine`, `web-archive`,
  `site-mirror`, `windows`, `python`, `tkinter`, `pyinstaller`, `website-downloader`.
- **About → Website**: ссылка на Releases.
- **Releases**: отметить `v2.4` как latest.
- Если планируете принимать чужие правки — добавьте `CONTRIBUTING.md` и шаблон issue.

## Чего делать не нужно

- Не коммитьте `dist/`, `.venv/`, `mirror/` — они в `.gitignore`.
- Не загружайте exe в сам репозиторий (ограничение 100 МБ на файл и раздувание
  истории): для бинарников есть Releases.
- Не публикуйте восстановленные копии чужих сайтов — репозиторий про инструмент,
  а не про контент.
