# Обновление и переработка лендинга lastcopy.ru и README.md (v3.6.6)

| Поле | Значение |
|---|---|
| Статус | выполнено |
| Версия | 1.0 |
| Дата | 15.11.2026 |
| Основание | запрос владельца: «Прочитай и проанализируй наш лендинг (https://lastcopy.ru/) переделай. Так же перепроверь переделай README на https://github.com/azxasd69/site-restorer/ Придерживайся лучших практик и подходов.» |
| Область | `D:\dev\sr-docs`: `README.md`, `landing/index.html`, `landing/en/index.html`, `landing/index.md`, `landing/en/index.md`, `landing/llms.txt`, `landing/assets/style.css`, `landing/guide/index.html`, `landing/guide/vosstanovit-sajt-iz-veb-arhiva/index.html`, `landing/en/guide/how-to-rebuild-a-website-from-the-wayback-machine/index.html`, `landing/support/index.html`, `landing/en/support/index.html`, `landing/sitemap.xml` |

---

## 1. Аудит исходного состояния (`lastcopy.ru` и `README.md`)

1. **Устаревшее содержание относительно релиза `v3.6.6`:**
   - В `landing/index.html`, `landing/en/index.html`, `landing/index.md`, `landing/en/index.md` и гайдах пользователям по-прежнему предлагался `python -m http.server 8080`, хотя в `3.6.6` появился встроенный сервер копии (`Д-49`, кнопка «Открыть копию» и `--serve`).
   - Преимущества и описания на лендинге не отражали ключевые доработки `3.6.6`: постоянный `keep-alive` пул, новый AIMD без ложных просадок на мелких файлах, автораспаковку скрытого `gzip` (`1f 8b`), поддомены (`--include-host`), минификаторы `?f=`, стратифицированный `--limit`, очистку `.part`, починку смешанного `windows-1251 + UTF-8` и отсев HTML-заглушек.
   - В `landing/index.md`, `landing/en/index.md` и `landing/llms.txt` в вводной части оставались устаревшие размеры `6.2 MB` / `1.8 MB` / `2.1 MB` / `7.5 MB` / `4.5 MB` времён `3.2.x`, а также устаревший список CLI-флагов.
   - В `README.md` отсутствовала верхняя оглавляющая навигация и блок «Быстрый старт» с `--serve`, а также неполный перечень флагов CLI (`--include-host`, `--rescue-budget`, `--text-limit`, `--serve`, `--port`) и модулей `src/` (`17` модулей с `serve.rs`).
2. **Доступность и вёрстка (`axe-core 4.12.1` + DOM):**
   - На `1440` и `390` `axe-core` выявлял `2` нарушения `aria-prohibited-attr` (`aria-label` на `<span class="lang">` и `<div class="folder-tree">` без роли).
   - При мобильном отображении (`<= 420 px`) ссылки `.header-support` и `.btn-small` теряли видимый текст (`display: none`), вызывая `link-name` нарушения у `axe-core` без `aria-label`.
   - Прокручиваемые блоки `<pre class="code-body">` не имели `tabindex="0"` (`scrollable-region-focusable`).
   - На карточках `#guides` оставались 4 инлайн-атрибута `style="margin-top:0"`.

---

## 2. Что изменено

### 2.1 Переработка `README.md`

- Добавлена шапка с навигацией по разделам и сворачиваемый блок `<details>` с полным набором скриншотов (`01-idle`, `02-inventory`, `03-download`, `04-done` на первом экране).
- Добавлен раздел **«Быстрый старт»** для GUI (с кнопкой **Открыть копию**) и CLI (`--cli` и `--serve`), актуальная таблица загрузок `v3.6.6` (`9,0 / 4,1 / 27,8 / 12,8 / 12,7 МБ`) со ссылками на `SHA256SUMS.txt` и `SBOM-site-restorer.cdx.json`.
- Обновлена таблица возможностей под `3.6.6`, приведена полная таблица всех `22` флагов CLI со значениями по умолчанию и перечень полей `RESULT {…}`, структура репозитория (`17` модулей `src/`), команды сборки и локальных гейтов (`150/147` тестов `nextest`) и обновлённый `English summary`.

### 2.2 Переработка лендинга `landing/` (`RU` + `EN`)

- **`landing/index.html` и `landing/en/index.html`:**
  - Обновлены `meta description`, JSON-LD (`SoftwareApplication`, `FAQPage`), `eyebrow` (`windows & linux · v3.6.6 · mit · без установки`), буллиты ценности («Встроенный сервер», «Один exe без Python», «Любая CMS»).
  - Переписаны карточки преимуществ (`01`–`06`), шаг `4` «Откройте копию и отчёт», блок кода и пояснения для встроенного сервера (`--serve D:\copy --port 8080`), перечень флагов CLI, дерево `site\` и ответ в FAQ про Python.
  - Устранены все нарушения `axe-core`: `<span class="lang">` заменён на `<nav class="lang">`, `<div class="folder-tree">` получил `role="region"`, ссылкам `.header-support` и кнопке скачивания в шапке добавлены `aria-label`, всем `<pre class="code-body">` добавлен `tabindex="0"`, убраны `style="margin-top:0"` в пользу класса `.h-card-top`.
- **`landing/assets/style.css` (`?v=6`):** добавлен класс `.h-card-top`, усилен `--lost-ink` до `#57626e`.
- **`landing/index.md`, `landing/en/index.md`, `landing/llms.txt`, гайды и `sitemap.xml`:** синхронизированы описания `v3.6.6`, встроенного сервера `--serve`, фактических размеров бинарников `v3.6.6`, перечня CLI-флагов и даты `lastmod` (`2026-11-15`) для главных страниц.

---

## 3. Проверки

| Проверка | Результат |
|---|---|
| Headless Chrome (`agent-browser` 0.42.0) — локальный HTTP-сервер `landing/` (`1440×900` и `390×844`, RU и EN) | Ошибок JS `0`, битых изображений `0`, горизонтальный скролл `docW == winW` (`1440`, `390`) на RU и EN |
| `axe-core 4.12.1` (`--tags wcag2a,wcag2aa`) на `1440` и `390` для RU и EN | **`violations: 0`** во всех 4 замерах (`passes: 29`) |
| `python scripts/check_downloads.py` | `релиз: v3.6.6, ассетов: 7`, `все ссылки сходятся с релизом` (`rc=0`) |
| `python tools/audit-docs.py` | `9` файлов, `0` дефектов (`README.md` 26 494 Б) |
| Скриншоты переделанной страницы | `D:\dev\sr-bench\lastcopy-after-1440.png` (1440×900), `D:\dev\sr-bench\lastcopy-after-390.png` (390×844) |

---

## 4. Сверка с постановкой

| Пункт постановки (дословно) | Где отражено | Чем подтверждено | Совпадает с результатом |
|---|---|---|---|
| «Прочитай и проанализируй наш лендинг (https://lastcopy.ru/)» | §1 | Чтение `https://lastcopy.ru/` (`fetch_content`), замеры в `agent-browser` (`lastcopy-before-*.png`, `axe-core`), разбор `landing/*` | да |
| «переделай.» | §§2.2, 3 | Обновлены `landing/{index.html,en/index.html,index.md,en/index.md,llms.txt,assets/style.css,guide/*,support/*,sitemap.xml}`, `axe-core` `0 violations` | да |
| «Так же перепроверь переделай README на https://github.com/azxasd69/site-restorer/» | §§1, 2.1, 3 | Полностью переработан `D:\dev\sr-docs\README.md` (`26 494 Б`, `audit-docs.py` код `0`) | да |
| «Придерживайся лучших практик и подходов.» | §§2.1–2.2, 3 | WCAG 2.0 AA (`axe-core 0`), адаптивность без переполнения (`390`/`1440`), единая структура README + Quick Start + таблицы флагов + скрытые второстепенные скриншоты | да |

**Что не проверено и почему:** живой визуальный рендер всех вторичных гайдов `/guide/*` (кроме 2 обновлённых гайдов и `/support/`, проверены статикой и общим CSS `style.css?v=6`) и тёмная тема (`prefers-color-scheme: dark`) в браузере. Деплой на `https://lastcopy.ru/` выполняется workflow GitHub Actions `Deploy landing` после пуша в `main`.
