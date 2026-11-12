#!/usr/bin/env python3
"""Проверка флагов сборки: единственный источник и запрещённые значения.

Зачем. Флаги базовой ISA заданы в `.cargo/config.toml`, но переменная окружения
`RUSTFLAGS` полностью перекрывает файл. Пока значения жили в двух местах,
правка в файле в конвейере не действовала — и это ничем не обнаруживалось
(ПР-15, Д-69). Скрипт закрывает класс дефектов:

1. в репозитории и конвейере нет запрещённых значений (`target-cpu=native`,
   `x86-64-v2/v3/v4`, глобальные `+avx/+avx2/+fma/+sse4.*`);
2. все цели, которые собирает конвейер, описаны в `.cargo/config.toml`;
3. конвейер не задаёт `RUSTFLAGS` для релизных ячеек матрицы.

Разрешено: `x86-64-v3` в задаче замеров `isa-bench` — это вариантная сборка
для сравнения чисел, а не поставка.

Запуск:  python3 scripts/check-flags.py [--json]
Выход:   0 — всё сходится, 1 — есть нарушения.
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

CONFIG = ROOT / ".cargo" / "config.toml"
WORKFLOW = ROOT / ".github" / "workflows" / "build.yml"

# Значения, запрещённые политикой (docs/PLATFORMS.md §2).
FORBIDDEN = [
    (re.compile(r"target-cpu\s*=\s*native"), "target-cpu=native"),
    (re.compile(r"x86-64-v2"), "x86-64-v2"),
    (re.compile(r"x86-64-v3"), "x86-64-v3"),
    (re.compile(r"x86-64-v4"), "x86-64-v4"),
    (re.compile(r"target-feature=\+avx\b"), "глобальный +avx"),
    (re.compile(r"target-feature=\+avx2\b"), "глобальный +avx2"),
    (re.compile(r"target-feature=\+fma\b"), "глобальный +fma"),
    (re.compile(r"target-feature=\+sse4"), "глобальный +sse4.*"),
]

# Файлы, где флаги имеют силу. Документация и планы не проверяются: там эти
# строки встречаются как запреты и как примеры.
CHECKED = [
    ".cargo/config.toml",
    ".github/workflows/build.yml",
    "Cargo.toml",
    "build.rs",
    "rust-toolchain.toml",
]

# Задача, которой вариантная сборка разрешена.
BENCH_JOB = "isa-bench"


def read(path: str) -> str:
    file = ROOT / path
    if not file.is_file():
        return ""
    return file.read_text(encoding="utf-8").replace("\r\n", "\n")


def workflow_jobs(text: str) -> dict[str, str]:
    """Грубый разбор задач: от «  имя:» до следующей задачи того же уровня."""
    jobs: dict[str, list[str]] = {}
    current: str | None = None
    in_jobs = False
    for line in text.split("\n"):
        if line.startswith("jobs:"):
            in_jobs = True
            continue
        if not in_jobs:
            continue
        match = re.match(r"^  ([a-zA-Z0-9_-]+):\s*$", line)
        if match:
            current = match.group(1)
            jobs[current] = []
            continue
        if current:
            jobs[current].append(line)
    return {name: "\n".join(lines) for name, lines in jobs.items()}


def main() -> int:
    as_json = "--json" in sys.argv
    problems: list[str] = []

    # 1. Запрещённые значения.
    for path in CHECKED:
        text = read(path)
        for line_no, line in enumerate(text.split("\n"), start=1):
            stripped = line.strip()
            if stripped.startswith("#"):
                continue
            for pattern, label in FORBIDDEN:
                if not pattern.search(line):
                    continue
                # Вариантная сборка для замеров — единственное исключение.
                if label.startswith("x86-64-v3") and BENCH_JOB in text:
                    block = workflow_jobs(text).get(BENCH_JOB, "")
                    if line in block or label in block:
                        continue
                problems.append(f"{path}:{line_no}: запрещено {label}")

    # 2. Цели конвейера описаны в файле.
    config = read(".cargo/config.toml")
    workflow = read(".github/workflows/build.yml")
    targets = set(re.findall(r"^\s+target:\s*(\S+)\s*$", workflow, re.M))
    for target in sorted(targets):
        if f"[target.{target}]" not in config:
            problems.append(
                f".cargo/config.toml: нет блока [target.{target}] для цели конвейера"
            )
    for target in targets:
        block = config.split(f"[target.{target}]", 1)
        if len(block) < 2:
            continue
        body = block[1].split("[target.", 1)[0]
        if "target-cpu=x86-64" not in body:
            problems.append(f".cargo/config.toml: у {target} не зафиксирована базовая ISA")
    if 'linker = "musl-gcc"' not in config:
        problems.append(".cargo/config.toml: для musl не задан линковщик musl-gcc")

    # 3. Конвейер не задаёт RUSTFLAGS релизным ячейкам.
    for name, block in workflow_jobs(workflow).items():
        # Комментарии не считаются: в них RUSTFLAGS упоминается как объяснение.
        code = "\n".join(
            line for line in block.split("\n") if not line.strip().startswith("#")
        )
        if "RUSTFLAGS" in code and name != "isa-bench":
            problems.append(
                f".github/workflows/build.yml: задача «{name}» задаёт RUSTFLAGS — "
                "флаги должны браться из .cargo/config.toml"
            )
    if re.search(r"^\s+rustflags:", workflow, re.M):
        problems.append(
            ".github/workflows/build.yml: в матрице остались rustflags — "
            "единственный источник это .cargo/config.toml"
        )

    if as_json:
        print(json.dumps({"problems": problems}, ensure_ascii=False, indent=2))
    elif problems:
        print(f"нарушений: {len(problems)}")
        for item in problems:
            print(f"  - {item}")
    else:
        print(
            "флаги в порядке: единственный источник — .cargo/config.toml, "
            f"запрещённых значений нет, целей в конвейере {len(targets)}"
        )
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
