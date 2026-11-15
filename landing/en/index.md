# Site Restorer — rebuild a website from the web archive

> Site Restorer (v3.6.6) is a portable Windows and Linux app that rebuilds a site from the Internet Archive: it inventories URLs via the CDX index, downloads files in parallel over a persistent keep-alive pool, unpacks hidden gzipped assets, rewrites links, serves the copy via a built-in HTTP server and writes a completeness report. Single self-contained Rust binary — no installer, no Python, MIT licensed.

Key point: the app recovers only what the archive actually saved; dynamic CMS features (cart, sign-in, search, AJAX) do not work in a static copy.

## What it is

- Purpose: restoring your own site when there are no files, no backups and no hosting access.
- Platform: Windows 10 (1809+) and Windows 11, x64. CLI and GUI builds for Linux (x64) are published too.
  From 3.6.1 only 64-bit builds are published; the last 32-bit build is 3.6.0.
- Implementation: Rust with an egui GUI, plus a CLI-only binary. The Python build (`legacy/python`) and the Go port (`ports/go`) are archived in the repository and no longer released.
- Sizes (v3.6.6): Windows GUI x64 — 9.0 MB, CLI — 4.1 MB, static Linux CLI — 12.8 MB, Linux GUI — 27.8 MB, Linux `.tar.gz` package — 12.7 MB.
- Distribution: GitHub Releases, with a `SHA256SUMS.txt` checksum file attached.
- License: MIT, source code is open.
- UI languages: Russian, English, switched on the fly.

## Advantages

Six things that decide whether a copy works out:

1. **Multithreaded download.** Persistent keep-alive pool (up to 24 workers) without extra TLS handshakes, AIMD driven by 429/5xx/latency without false drops on small files, parallel CDX paging and up to 4-thread link rescue, `Retry-After` pauses.
2. **Automatic local assembly.** Automatic hidden `gzip` (`1f 8b`) decompression for CSS/JS/SVG, asset subdomains (`--include-host`), `?f=` minifier bundling, root-relative miss promotion to `https://<host>...` and built-in HTTP server (`--serve`).
3. **Honest completeness report.** Recovered, never-archived and failed counts kept apart; `--serve` launch banner, `N of M` index pages, decompression/mismatch counters and missing-CSS warning.
4. **Addresses from every archived year.** Up to 50,000 addresses and 200 index pages, stratified `--limit` sampling across sections, neighbouring-year fallbacks and prioritised asset rescue.
5. **Resume and re-run.** Done files are skipped, stale `.part` leftovers are cleaned on start, mid-stream read drops are retried automatically, and metadata is flushed periodically.
6. **Encodings and file integrity.** BOM, declared charset and statistical detection, double-encoding and mixed `windows-1251 + UTF-8` repair, rejection of HTML error pages disguised as icons/fonts/CSS.

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
- Built-in `127.0.0.1` HTTP server (`Range`, `ETag`, `?query` routing): **Open copy** button in the UI or `--serve <folder> [--port N]` in CLI.
- CLI: `--site`, `--out`, `--workers`, `--include-host`, `--rate`, `--limit`, `--rescue-budget`, `--serve`, `--port`, `--lang`, `--version`.

## How it works

1. Paste a site address — a domain, a link with tracking parameters or an archive URL. Input is normalised automatically.
2. Choose an output folder (default `Documents\SiteRestore\site`).
3. Press "Start". Downloading runs in the background: progress by file type, request log, a "Stop" button.
4. Click **“Open copy”** to start the built-in local server on `127.0.0.1` and browse the site; `report.html` breaks down coverage and broken links.

Output structure:

```
<folder>/
├── site/        the browsable copy (“Open copy” button or --serve)
├── mirror/      raw files from the archive
├── data/        inventory.jsonl — the URL list
└── report.html  coverage report
```

Serving locally with the built-in server (no Python required):

```powershell
SiteRestorer-windows-x64-cli.exe --serve D:\copy --port 8080
# http://127.0.0.1:8080/
```

Command line:

```powershell
SiteRestorer-windows-x64-cli.exe --cli --site example.com --out D:\copy --workers 8
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

- x64 GUI (Rust + egui, 9.0 MB): https://github.com/azxasd69/site-restorer/releases/latest/download/SiteRestorer-windows-x64.exe
- 32-bit GUI (5.6 MB, 3.6.0, no longer built): https://github.com/azxasd69/site-restorer/releases/download/v3.6.0/SiteRestorer-windows-x86.exe
- Windows x64 CLI (4.1 MB): https://github.com/azxasd69/site-restorer/releases/latest/download/SiteRestorer-windows-x64-cli.exe
- Windows 32-bit CLI (3.6.0): https://github.com/azxasd69/site-restorer/releases/download/v3.6.0/SiteRestorer-windows-x86-cli.exe
- Linux x64 CLI, static (12.8 MB): https://github.com/azxasd69/site-restorer/releases/latest/download/SiteRestorer-linux-x64-cli
- Linux x64 GUI (27.8 MB): https://github.com/azxasd69/site-restorer/releases/latest/download/SiteRestorer-linux-x64-gui
- Linux package (binaries + install.sh + .desktop, 12.7 MB): https://github.com/azxasd69/site-restorer/releases/latest/download/SiteRestorer-linux-x64.tar.gz
- Checksums: https://github.com/azxasd69/site-restorer/releases/latest/download/SHA256SUMS.txt
- All releases: https://github.com/azxasd69/site-restorer/releases

The build is not signed with a paid certificate, so Windows SmartScreen may warn: choose "More info" → "Run anyway" (or run `Unblock.cmd` from the archive). The source code and CI are public — build matrix in `BUILD.md`.

## Support the author

No ads, subscriptions or telemetry: https://boosty.to/explosion/donate

The site also has a “Support” link in the header and under the advantages, a “Say thanks” block with a QR code in the download section, a dedicated section and a footer link.

## FAQ

**Do I need Python?** No — the primary build is a single Rust binary with no interpreter or dependencies. A local HTTP server is built right in (**Open copy** button or `--serve`), so no external server or Python is needed.

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
