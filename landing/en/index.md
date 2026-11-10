# Site Restorer — rebuild a website from the web archive

> Site Restorer is a portable Windows app that rebuilds a site from the Internet Archive: it inventories captured URLs via the CDX index, downloads files in parallel, rewrites links to relative paths, checks for broken references and writes a coverage report. The primary build is a single self-contained Rust binary — no installer, no runtime, no dependencies. MIT licensed. UI in Russian and English.

Key point: the app recovers only what the archive actually saved; dynamic CMS features (cart, sign-in, search, AJAX) do not work in a static copy.

## What it is

- Purpose: restoring your own site when there are no files, no backups and no hosting access.
- Platform: Windows 10 (1809+) and Windows 11, x64 and x86. CLI and GUI builds for Linux (x64) are published too.
- Implementation: Rust with an egui GUI, plus a CLI-only binary. The Python build (`legacy/python`) and the Go port (`ports/go`) are archived in the repository and no longer released.
- Sizes: Windows GUI x64 — 6.2 MB, x86 — 5.6 MB, CLI — 1.8 MB (x86 — 1.3 MB), static Linux CLI — 2.1 MB, Linux GUI — 7.5 MB, Linux `.tar.gz` package — 4.5 MB.
- Distribution: GitHub Releases, with a `SHA256SUMS.txt` checksum file attached.
- License: MIT, source code is open.
- UI languages: Russian, English, switched on the fly.

## Advantages

Six things that decide whether a copy works out:

1. **Multithreaded download.** Up to 24 workers, adaptive pace driven by the archive's response, `Retry-After` pauses instead of failed files, optional rate limit. 1,200 files ≈ 4 minutes.
2. **Automatic local assembly.** Links in attributes, image source sets, styles and scripts are rewritten to relative paths and the base tag is removed — the copy opens without the original domain.
3. **Honest completeness report.** Recovered, never-archived and failed counts are kept apart; coverage by file type, size from the actual files, broken links, and a warning that the copy is static.
4. **Addresses from every archived year.** Up to 50,000 addresses and 200 index pages, streamed index response, neighbouring years tried, truncation stated explicitly.
5. **Resume and re-run.** A file counts as done only when it exists and is not empty, unfinished files stay under a temporary name, and a newer version reads the same folder and inventory.
6. **Encodings and file integrity.** Byte order mark, declared charset and statistical content assessment, double-encoding repair; binary files behind a page address are copied untouched.

## Features

- Archive inventory: Internet Archive CDX index, paged crawl, up to 50,000 URLs per run (caps: 50,000 URLs and 200 CDX pages).
- Parallel downloading of HTML, CSS/JS, images and other files with AIMD adaptive concurrency: 2 → 24 workers, backing off on HTTP 429.
- Rate-limit handling: `Retry-After`, exponential backoff, a circuit breaker for 429 bursts, keep-alive.
- Resume: nothing already downloaded is requested twice; an interrupted run continues where it stopped.
- Offline build: absolute links rewritten to relative ones; heuristics for CMS caches with build hashes and for image thumbnails.
- Verification and report: pages are scanned, internal and broken links counted, `report.html` shows coverage by file type, total size and errors.
- Encodings: pages and stylesheets saved in cp1251 (or another declared encoding) are converted to UTF-8 and `<meta charset>` is fixed.
- Address paste: Ctrl+V, Ctrl+Shift+V, Shift+Insert (Linux), multi-line browser payload is parsed down to the host; Enter starts, Esc unfocuses.
- Native folder picker and RU/EN switching on the fly.
- Any address format: links from email, search or the archive, wrong keyboard layout, IDN domains.
- Any CMS: tested on Bitrix, WordPress, MODX, Craft CMS, Laravel.
- Diagnostics: a log entry with URL, HTTP status, size and timing for every request.
- CLI: `--site`, `--out`, `--workers`, `--rate`, `--limit`, `--lang`, `--version`.

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
SiteRestorer-windows-x64.exe --cli --site example.com --out D:\mirror --workers 16
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

- x64 GUI (Rust + egui, 6.2 MB): https://github.com/azxasd69/site-restorer/releases/latest/download/SiteRestorer-windows-x64.exe
- x86 GUI (5.6 MB): https://github.com/azxasd69/site-restorer/releases/latest/download/SiteRestorer-windows-x86.exe
- Windows x64 CLI (1.8 MB): https://github.com/azxasd69/site-restorer/releases/latest/download/SiteRestorer-windows-x64-cli.exe
- Windows x86 CLI (1.3 MB): https://github.com/azxasd69/site-restorer/releases/latest/download/SiteRestorer-windows-x86-cli.exe
- Linux x64 CLI, static (2.1 MB): https://github.com/azxasd69/site-restorer/releases/latest/download/SiteRestorer-linux-x64-cli
- Linux x64 GUI (7.5 MB): https://github.com/azxasd69/site-restorer/releases/latest/download/SiteRestorer-linux-x64-gui
- Linux package (binaries + install.sh + .desktop, 4.5 MB): https://github.com/azxasd69/site-restorer/releases/latest/download/SiteRestorer-linux-x64.tar.gz
- Checksums: https://github.com/azxasd69/site-restorer/releases/latest/download/SHA256SUMS.txt
- All releases: https://github.com/azxasd69/site-restorer/releases

The build is not signed with a paid certificate, so Windows SmartScreen may warn: choose "More info" → "Run anyway" (or run `Unblock.cmd` from the archive). The source code and CI are public — build matrix in `BUILD.md`.

## Support the author

No ads, subscriptions or telemetry: https://boosty.to/explosion/donate

The site also has a “Support” link in the header and under the advantages, a “Say thanks” block with a QR code in the download section, a dedicated section and a footer link.

## FAQ

**Do I need Python?** No — the primary build is a single Rust binary with no interpreter. Python is only useful for `python -m http.server`; any static server will do.

**What if the archive saved nothing?** The report will say so; the app does not invent content. Try domain variants (with and without `www`) and nearby years: the closest snapshots are probed for every URL.

**How long does a restore take?** Depends on the site and archive limits: minutes for a small site, hours for thousands of URLs. Progress and timing are shown in the window; resuming lets you continue later.

**Will product images come back?** Only if the archive captured them: crawlers often save pages but not every image. In one case 2,510 images were missing entirely while the text survived.

**Will the cart, sign-in and search work?** No, the copy is static.

**Is this legal?** The tool is built for restoring your own sites; it does not bypass protection or spoof the User-Agent. Publishing someone else's copy is not allowed. The app is MIT licensed.

## Links

- Landing (RU): https://lastcopy.ru/
- Landing (EN): https://lastcopy.ru/en/
- Repository: https://github.com/azxasd69/site-restorer
- README: https://github.com/azxasd69/site-restorer/blob/main/README.md
- Architecture: https://github.com/azxasd69/site-restorer/blob/main/ARCHITECTURE.md
- Build matrix: https://github.com/azxasd69/site-restorer/blob/main/BUILD.md
- Changelog: https://github.com/azxasd69/site-restorer/blob/main/CHANGELOG.md
- Issues: https://github.com/azxasd69/site-restorer/issues
- MIT license: https://github.com/azxasd69/site-restorer/blob/main/LICENSE

## Background

The project started after the Beget hosting outage on the night of 1–2 November 2026, when the provider confirmed the loss of customer data (virtual servers, S3, Kubernetes, VPS backups). The site this project began with was hosted there and had no backups of its own — the web archive remained the only source of content.
