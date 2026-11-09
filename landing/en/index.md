# Site Restorer — rebuild a website from the web archive

> Site Restorer is a portable Windows app that rebuilds a site from the Internet Archive: it inventories captured URLs via the CDX index, downloads files in parallel, rewrites links to relative paths, checks for broken references and writes a coverage report. A single executable, no installer and no Python. MIT licensed. UI in Russian and English.

Key point: the app recovers only what the archive actually saved; dynamic CMS features (cart, sign-in, search, AJAX) do not work in a static copy.

## What it is

- Purpose: restoring your own site when there are no files, no backups and no hosting access.
- Platform: Windows 10 (1809+) and Windows 11, x64 and x86.
- Distribution: portable `SiteRestorer.exe` (x64 ≈ 12 MB, x86 ≈ 10 MB), no installation.
- Stack: Python bundled into an executable with PyInstaller; Tcl/Tk included.
- License: MIT, source code is open.
- UI languages: Russian, English.

## Features

- Archive inventory: Internet Archive CDX index, paged crawl, up to 50,000 URLs per run (caps: 50,000 URLs and 200 CDX pages).
- Parallel downloading of HTML, CSS/JS, images and other files with AIMD adaptive concurrency: 2 → 24 workers, backing off on HTTP 429.
- Rate-limit handling: `Retry-After`, exponential backoff, a circuit breaker for 429 bursts, keep-alive.
- Resume: nothing already downloaded is requested twice; an interrupted run continues where it stopped.
- Offline build: absolute links rewritten to relative ones; heuristics for CMS caches with build hashes and for image thumbnails.
- Verification and report: pages are scanned, internal and broken links counted, `report.html` shows coverage by file type, total size and errors.
- Any address format: links from email, search or the archive, wrong keyboard layout, IDN domains.
- Any CMS: tested on Bitrix, WordPress, MODX, Craft CMS, Laravel.
- Diagnostics: a log entry with URL, HTTP status, size and timing for every request.
- CLI: `--site`, `--out`, `--workers`, `--rate`, `--limit`, `--lang`.

## How it works

1. Paste a site address — a domain, a link with tracking parameters or an archive URL. Input is normalised automatically.
2. Choose an output folder (default `Documents\SiteRestore\site`).
3. Press "Start". Downloading runs in the background: progress by file type, request log, a "Stop" button.
4. Open `report.html` — coverage, size, broken links. Serve the copy with a local static server.

Output structure:

```
<folder>/
├── site/        the browsable copy (serve it locally)
├── mirror/      raw files from the archive
├── data/        inventory.jsonl — the URL list
└── report.html  coverage report
```

Serving the copy locally:

```powershell
cd Documents\SiteRestore\site
python -m http.server 8080
# http://127.0.0.1:8080/
```

Command line:

```powershell
SiteRestorer.exe --cli --site example.com --out D:\mirror --workers 16
```

## Case study

A trading company's storefront: product catalogue, blog and PDF catalogues. No files, no backups, no hosting access left — the web archive was the only source.

| Metric | Value |
|---|---|
| Unique URLs in the CDX index | 1,638 |
| Snapshots, 2016–2026 | 9,476 |
| Downloaded | 1,609 of 1,638 (98.2%) |
| HTML pages | 1,164 of 1,168 |
| Catalogue pages | 1,042 of 1,044 |
| Copy size | 315 MB |

What did not come back:

- Product images: 2,510 images were never captured (no CDX records, replay returns 404 for every year).
- CMS dynamics: cart, sign-in, search and AJAX catalogue filters need a backend and a database.
- Template cache: asset filenames carry a build hash; a file of the same type is substituted — styles survive, the exact version may differ.

## Limits and ethics

- Only what the archive saved can be recovered.
- The copy is static: forms, sign-in, cart, search and AJAX do not work.
- The Internet Archive throttles requests from a single IP; large sites take hours, resuming helps.
- The tool is meant for archiving and restoring your own sites. It does not bypass protection, spoof the User-Agent or disguise itself as a browser. Publishing a copy of someone else's site is not allowed; respect the Internet Archive terms and content owners' rights.

## Download

- x64: https://github.com/azxasd69/site-restorer/releases/latest/download/SiteRestorer.exe
- x86: https://github.com/azxasd69/site-restorer/releases/latest/download/SiteRestorer-x86.exe
- All releases: https://github.com/azxasd69/site-restorer/releases

The build is not signed with a paid certificate, so Windows SmartScreen may warn: choose "More info" → "Run anyway" (or run `Unblock.cmd` from the archive). The source code and CI are public — you can build it yourself.

## Support the author

No ads, subscriptions or telemetry: https://boosty.to/explosion/donate

## FAQ

**Do I need Python?** No — the interpreter and Tcl/Tk are bundled. Python is only useful for `python -m http.server`; any static server will do.

**What if the archive saved nothing?** The report will say so; the app does not invent content. Try domain variants (with and without `www`) and nearby years: the closest snapshots are probed for every URL.

**How long does a restore take?** Depends on the site and archive limits: minutes for a small site, hours for thousands of URLs. Progress and timing are shown in the window; resuming lets you continue later.

**Will product images come back?** Only if the archive captured them: crawlers often save pages but not every image. In one case 2,510 images were missing entirely while the text survived.

**Will the cart, sign-in and search work?** No, the copy is static.

**Is this legal?** The tool is built for restoring your own sites; it does not bypass protection or spoof the User-Agent. Publishing someone else's copy is not allowed. The app is MIT licensed.

## Links

- Landing (RU): https://azxasd69.github.io/site-restorer/
- Landing (EN): https://azxasd69.github.io/site-restorer/en/
- Repository: https://github.com/azxasd69/site-restorer
- README: https://github.com/azxasd69/site-restorer/blob/main/README.md
- Architecture: https://github.com/azxasd69/site-restorer/blob/main/ARCHITECTURE.md
- Changelog: https://github.com/azxasd69/site-restorer/blob/main/CHANGELOG.md
- Issues: https://github.com/azxasd69/site-restorer/issues
- MIT license: https://github.com/azxasd69/site-restorer/blob/main/LICENSE

## Background

The project started after the Beget hosting outage on the night of 1–2 November 2026, when the provider confirmed the loss of customer data (virtual servers, S3, Kubernetes, VPS backups). The site this project began with was hosted there and had no backups of its own — the web archive remained the only source of content.
