#!/usr/bin/env python3
"""Проверка ссылок скачивания на лендинге против фактического релиза.

Ловит класс дефектов, который уже случался: имя файла на странице не совпадает
с ассетом в релизе (404) и ссылка отдаёт не тот размер.

Запуск:  python scripts/check_downloads.py
Выход:   0 — всё сходится, 1 — есть расхождения, 2 — не удалось получить релиз.
Токен:   GITHUB_TOKEN (в Actions выдаётся автоматически, read-only достаточно).
"""

from __future__ import annotations

import json
import os
import re
import sys
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LANDING = ROOT / "landing"
SCAN_SUFFIXES = {".html", ".md", ".txt"}
SKIP_DIRS = {"node_modules", ".git", "dist"}

# releases/latest/download/<asset>  и  releases/download/<tag>/<asset>
LATEST_RE = re.compile(r"releases/latest/download/([A-Za-z0-9._+-]+)")
# Тег захватываем: ссылка на конкретную версию (например, на последнюю 32-битную
# сборку 3.6.0) проверяется по СВОЕМУ релизу, а не по последнему. Иначе любая
# архивная ссылка считалась бы ошибкой после выхода следующей версии.
TAGGED_RE = re.compile(r"releases/download/([^/\s)\"'<>]+)/([A-Za-z0-9._+-]+)")

USER_AGENT = "site-restorer-link-check"


def iter_landing_files():
    for path in LANDING.rglob("*"):
        if not path.is_file() or path.suffix not in SCAN_SUFFIXES:
            continue
        if SKIP_DIRS & set(path.parts):
            continue
        yield path


def collect_references():
    """Возвращает (ссылки на последний релиз, ссылки на конкретные теги)."""
    latest: dict[str, set[str]] = {}
    tagged: dict[tuple[str, str], set[str]] = {}
    for path in iter_landing_files():
        try:
            text = path.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            continue
        rel = path.relative_to(ROOT).as_posix()
        for match in LATEST_RE.finditer(text):
            latest.setdefault(match.group(1), set()).add(rel)
        for match in TAGGED_RE.finditer(text):
            tagged.setdefault((match.group(1), match.group(2)), set()).add(rel)
    return latest, tagged


def api_get(url: str, token: str | None):
    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    if token:
        request.add_header("Authorization", f"Bearer {token}")
    request.add_header("Accept", "application/vnd.github+json")
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.load(response)


def head_size(url: str):
    request = urllib.request.Request(url, method="HEAD", headers={"User-Agent": USER_AGENT})
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            if response.status != 200:
                return None
            length = response.headers.get("Content-Length")
            return int(length) if length else None
    except urllib.error.HTTPError as error:
        print(f"  HEAD {url} -> HTTP {error.code}")
        return None
    except urllib.error.URLError as error:
        print(f"  HEAD {url} -> {error.reason}")
        return None


def main() -> int:
    token = os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN")
    repo = os.environ.get("GITHUB_REPOSITORY", "azxasd69/site-restorer")

    try:
        release = api_get(f"https://api.github.com/repos/{repo}/releases/latest", token)
    except Exception as error:  # noqa: BLE001 - причина важнее типа
        print(f"не удалось получить последний релиз: {error}")
        return 2

    tag = release.get("tag_name", "?")
    assets = {asset["name"]: asset for asset in release.get("assets", [])}
    print(f"релиз: {tag}, ассетов: {len(assets)}")

    refs, tagged = collect_references()
    print(f"ссылок на последний релиз: {len(refs)}, на конкретные теги: {len(tagged)}")

    problems: list[str] = []

    # 1. Каждый упомянутый файл должен существовать в последнем релизе.
    for name, where in sorted(refs.items()):
        if name not in assets:
            problems.append(
                f"нет ассета «{name}» в релизе {tag} (ссылка в: {', '.join(sorted(where))})"
            )

    # 2. Ссылка releases/latest/download/<asset> должна отдавать тот же размер.
    for name, asset in sorted(assets.items()):
        if name not in refs:
            continue
        url = f"https://github.com/{repo}/releases/latest/download/{name}"
        size = head_size(url)
        if size is None:
            problems.append(f"ссылка не отвечает 200: {name}")
        elif size != asset["size"]:
            problems.append(
                f"размер не совпадает для {name}: ссылка отдаёт {size} Б, в релизе {asset['size']} Б"
            )

    # 2а. Ссылки на конкретный тег проверяются по релизу этого тега: архивные
    # ссылки (последняя 32-битная сборка) живут дольше последнего релиза.
    releases_by_tag: dict[str, dict[str, dict]] = {}
    for (tag_ref, name), where in sorted(tagged.items()):
        if tag_ref not in releases_by_tag:
            try:
                data = api_get(f"https://api.github.com/repos/{repo}/releases/tags/{tag_ref}", token)
            except Exception as error:  # noqa: BLE001 - тег важнее типа ошибки
                problems.append(
                    f"не удалось получить релиз {tag_ref} (ссылка на «{name}» в: "
                    f"{', '.join(sorted(where))}): {error}"
                )
                releases_by_tag[tag_ref] = {}
                continue
            releases_by_tag[tag_ref] = {a["name"]: a for a in data.get("assets", [])}
        assets_of_tag = releases_by_tag[tag_ref]
        if name not in assets_of_tag:
            problems.append(
                f"нет ассета «{name}» в релизе {tag_ref} (ссылка в: {', '.join(sorted(where))})"
            )
            continue
        url = f"https://github.com/{repo}/releases/download/{tag_ref}/{name}"
        size = head_size(url)
        if size is None:
            problems.append(f"ссылка не отвечает 200: {tag_ref}/{name}")
        elif size != assets_of_tag[name]["size"]:
            problems.append(
                f"размер не совпадает для {tag_ref}/{name}: ссылка отдаёт {size} Б, "
                f"в релизе {assets_of_tag[name]['size']} Б"
            )

    # 3. Ассеты без ссылок с лендинга — не ошибка, но полезно знать.
    unused = sorted(set(assets) - set(refs))
    if unused:
        print(f"ассеты без ссылок с лендинга: {', '.join(unused)}")

    if problems:
        print(f"\nрасхождений: {len(problems)}")
        for item in problems:
            print(f"  - {item}")
        return 1

    print("\nвсе ссылки сходятся с релизом")
    return 0


if __name__ == "__main__":
    sys.exit(main())
