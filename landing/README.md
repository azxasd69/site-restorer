# Site Restorer — лендинг

Одностраничный сайт о программе: русская версия в корне, английская — в `en/`.
Без сборщика, без зависимостей и без внешних шрифтов: обычные HTML + CSS + 2 КБ JS.

```
landing/
├── index.html            RU
├── en/index.html         EN
├── assets/
│   ├── style.css         дизайн-система: токены, светлая/тёмная тема, адаптив
│   ├── app.js            меню, копирование команд, год в подвале
│   ├── favicon.svg       иконка (векторная)
│   └── img/              скриншоты приложения, OG-картинки, touch-icon
├── robots.txt
└── README.md             этот файл
```

## Локальный просмотр

Открыть `index.html` файлом можно, но лучше поднять статический сервер —
иначе относительные пути в `en/` и кэш ведут себя непредсказуемо:

```powershell
cd landing
C:\Python314\python.exe -m http.server 8090
# http://127.0.0.1:8090/      — RU
# http://127.0.0.1:8090/en/   — EN
```

## Публикация

### GitHub Pages (рекомендуется)

Важно: GitHub Pages при деплое «из ветки» публикует только корень репозитория или
папку `/docs` — произвольную папку `/landing` выбрать нельзя. Поэтому для этого
лендинга нужен деплой через GitHub Actions.

Готовая заготовка workflow лежит в `deploy/pages-workflow.yml`. Порядок:

1. Скопировать её в репозиторий: `.github/workflows/pages.yml`.
2. Settings → Pages → Build and deployment → Source: **GitHub Actions**.
3. Закоммитить и запушить. Workflow публикует содержимое `landing/` как артефакт,
   поэтому структура сайта сохраняется как есть.
4. Через минуту сайт будет на `https://azxasd69.github.io/site-restorer/`,
   английская версия — `/site-restorer/en/`.

Относительные ссылки внутри сайта работают и под подпапкой `/site-restorer/`,
править пути не нужно. Для своего домена — CNAME и DNS (см. ниже).

### Свой домен

1. Скопировать содержимое `landing/` в корень веб-сервера (или в `current/`
   при деплое через `tools\vps\deploy.ps1`).
2. Прописать домен в блоке «после публикации».
3. Для HTTPS — сертификат (Let's Encrypt / certbot).
4. Для GitHub Pages + свой домен: файл `CNAME` с доменом в `landing/`
   и DNS-записи `A`/`CNAME` по документации GitHub.

## Чек-лист после публикации (домен известен)

Сайт уже опубликован на GitHub Pages: `https://azxasd69.github.io/site-restorer/`.
Canonical, `hreflang`, абсолютные `og:url` / `og:image` / `twitter:image`, `sitemap.xml`
и строка `Sitemap:` в `robots.txt` уже прописаны под этот адрес.

Если сайт переедет на свой домен (или в другую подпапку), нужно заменить базовый URL
(он встречается 4 раза на страницу + `sitemap.xml` + `robots.txt`):

- [ ] `index.html`, `en/index.html`: `canonical`, три `hreflang`, `og:url`, `og:image`, `twitter:image`.
- [ ] `sitemap.xml`: оба `<loc>` и `xhtml:link`.
- [ ] `robots.txt`: строка `Sitemap:`.
- [ ] При переезде на свой домен — файл `CNAME` с доменом в `landing/` и DNS-записи по документации GitHub.
- [ ] Проверить превью: Telegram, X, VK, LinkedIn (кэш обновляется принудительно).

Важно про `robots.txt`: для project pages (`/<repo>/`) файл лежит в подпапке и
роботами не читается — они запрашивают `https://azxasd69.github.io/robots.txt`.
На индексацию это влияет слабо (страницы не закрыты), а `sitemap.xml` можно
отправить в Search Console вручную. Для своего домена ограничение исчезает.

## Как обновлять

| Что | Где |
|---|---|
| Версия, размер exe | `index.html` / `en/index.html`, блок `#download` (тексты «≈ 12 МБ») |
| Скриншоты | `assets/img/screenshot-*.png` — копии из `docs/screenshots/` репозитория |
| Цифры кейса | блоки `.stats` и `#case` в обеих языковых версиях |
| OG-картинки | `assets/img/og-ru.png`, `og-en.png` (1200×630) |

Ссылки на скачивание не содержат номера версии:

```
https://github.com/azxasd69/site-restorer/releases/latest/download/SiteRestorer.exe
https://github.com/azxasd69/site-restorer/releases/latest/download/SiteRestorer-x86.exe
```

Они всегда ведут на последний релиз, поэтому обновлять их при выходе новой версии
не нужно — только подпись с размером файла, если он заметно изменился.

## Для LLM и агентов

`llms.txt` — предложение Jeremy Howard (Answer.AI) от 03.09.2024, текущая редакция — v2 (10.08.2026), [llmstxt.org](https://llmstxt.org/).
Это **не стандарт** W3C/IETF: обязателен только `H1`, блок-описание и списки ссылок — конвенция.
Google официально заявил (15.06.2026), что файл не нужен для поиска и не влияет на видимость
([Search updates](https://developers.google.com/search/updates)); OpenAI, Anthropic и Perplexity
публичного подтверждения чтения не давали. Поэтому файл — дешёвая страховка для агентов и
инструментов (Chrome Lighthouse проверяет его в agentic browsing checks), а не рычаг SEO.

| Файл | Что это |
|---|---|
| `/llms.txt` | Карта сайта: H1, блок-описание, разделы со списками `- [имя](url): пояснение` и `## Optional` |
| `/index.md` | Русская страница в чистом markdown (без навигации и скриптов) |
| `/en/index.md` | Английская страница в markdown |

В `<head>` обеих страниц — `<link rel="describedby" href="llms.txt">` и
`<link rel="alternate" type="text/markdown" href="index.md">` — ровно эти два rel
рекомендует v2. Также добавлен JSON-LD: `SoftwareApplication` и `FAQPage`.

Честные оговорки по разметке:

- Rich snippet для `SoftwareApplication` требует `aggregateRating` или `review`. Рейтинга у
  проекта нет, выдумывать его нельзя — разметка остаётся для машинного понимания, но
  расширенного сниппета в Google не даст.
- FAQ rich results с мая 2026 не показываются никому (документация Google удалена 15.06.2026),
  `FAQPage` оставлен только как машиночитаемая структура вопросов.

`robots.txt` разрешает всё (`Allow: /`) — AI-краулеры не блокируются: проект открытый, ему
нужна видимость в ответах ассистентов. Актуальные user-agent'ы: `GPTBot`, `OAI-SearchBot`,
`ChatGPT-User`, `ClaudeBot`, `Claude-User`, `Claude-SearchBot`, `PerplexityBot`,
`Google-Extended` (только обучение Gemini, не поиск), `CCBot`, `Applebot-Extended`.
Устарели и не используются: `anthropic-ai`, `Claude-Web`.

**Правило поддержки:** при правке текста страницы обновлять и соответствующий `.md`.
Забыть легко — поэтому `llms.txt` и `.md` лежат в репозитории рядом и ревьюятся вместе с HTML.

## Дизайн-решения

- **Палитра.** Тёплый бумажный фон `#f7f5f1`, графитовый текст, один приглушённый
  хвойный акцент `#2e5f55`. Никаких градиентов и «маркетингового» оранжевого.
- **Шрифты.** Системный стек (Segoe UI / Inter / system-ui): ноль сетевых запросов,
  мгновенный first paint, корректная кириллица.
- **Тёмная тема** — автоматическая через `prefers-color-scheme`, без переключателя.
- **Доступность.** Skip-link, семантика (`header/nav/main/section/footer`),
  `aria-expanded` у меню, alt у скриншотов, focus-visible, контраст ≥ 4.5:1
  для текста, отключение анимаций при `prefers-reduced-motion`.
- **Без JS** страница остаётся читаемой: меню на мобильном скрыто, но все разделы
  доступны по якорям из подвала; кнопки копирования просто не работают.
