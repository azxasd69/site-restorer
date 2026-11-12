#!/usr/bin/env python3
"""Проверка флагов сборки: единственный источник и запрещённые значения.

Зачем. Флаги базовой ISA заданы в `.cargo/config.toml`, но переменная окружения
`RUSTFLAGS` полностью перекрывает файл. Пока значения жили в двух местах,
правка в файле в конвейере не действовала — и это ничем не обнаруживалось
(ПР-15, Д-69). Скрипт закрывает класс дефектов:

1. в конфигурации сборки нет запрещённых значений (`target-cpu=native`,
   `x86-64-v2/v3/v4`, глобальные `+avx*`, `+fma`, `+sse4.*`);
2. все цели, которые собирает конвейер, описаны в `.cargo/config.toml`;
3. конвейер не задаёт `RUSTFLAGS` ни в одной задаче, кроме замеров;
4. проверка возможностей и `#[target_feature]` встречаются только в `src/isa.rs`:
   иначе выключатель `SR_NO_SIMD` обходится новым ускоренным путём в любом файле.

Исключение ровно одно и проверяется по строке, а не по всему файлу: вариантная
сборка `x86-64-v3` разрешена только в задаче `isa-bench` (числа для сравнения,
а не поставка).

Скрипт падает, если файл конфигурации или конвейера не найден: пропуск проверки
из-за отсутствующего файла — это ложноотрицательный результат, а не успех.

Запуск:  python3 scripts/check-flags.py [--json]
Выход:   0 — всё сходится, 1 — есть нарушения.
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

CONFIG = ".cargo/config.toml"
WORKFLOWS_DIR = ".github/workflows"

# Значения, запрещённые политикой (docs/PLATFORMS.md §2). Границы слов здесь
# не годятся: `+avx512f` должен ловиться так же, как `+avx`.
FORBIDDEN = [
    (re.compile(r"target-cpu\s*=\s*native"), "target-cpu=native"),
    (re.compile(r"x86-64-v2"), "x86-64-v2"),
    (re.compile(r"x86-64-v3"), "x86-64-v3"),
    (re.compile(r"x86-64-v4"), "x86-64-v4"),
    (re.compile(r"target-feature=\+avx"), "глобальный +avx*"),
    (re.compile(r"target-feature=\+fma"), "глобальный +fma"),
    (re.compile(r"target-feature=\+sse4"), "глобальный +sse4.*"),
]

# Файлы, где флаги имеют силу. Документация и планы не проверяются: там эти
# строки встречаются как запреты и как примеры.
CHECKED = ["Cargo.toml", "build.rs", "rust-toolchain.toml"]

# Задача, которой вариантная сборка разрешена.
BENCH_JOB = "isa-bench"


def read(path: str) -> str | None:
    file = ROOT / path
    if not file.is_file():
        return None
    return file.read_text(encoding="utf-8").replace("\r\n", "\n")


def job_of_lines(text: str) -> list[str | None]:
    """Для каждой строки — имя задачи, к которой она относится (или None).

    Разбор намеренно простой: задачи верхнего уровня в GitHub Actions — это
    отступ в два пробела внутри `jobs:`. Всё, что выше `jobs:`, помечается как
    «вне задачи» и тоже проверяется: `env:` верхнего уровня действует на все
    шаги сразу.
    """
    owners: list[str | None] = []
    current: str | None = None
    in_jobs = False
    for line in text.split("\n"):
        if line.rstrip() == "jobs:":
            in_jobs = True
            owners.append(None)
            continue
        if in_jobs:
            match = re.match(r"^  ([A-Za-z0-9_-]+):\s*$", line)
            if match:
                current = match.group(1)
                owners.append(current)
                continue
        owners.append(current)
    return owners


def main() -> int:
    as_json = "--json" in sys.argv
    problems: list[str] = []

    config = read(CONFIG)
    if config is None:
        problems.append(f"{CONFIG}: файл не найден — проверять нечего")
        config = ""
    if not (ROOT / WORKFLOWS_DIR).is_dir():
        problems.append(f"{WORKFLOWS_DIR}: каталог не найден — проверять нечего")

    workflows: dict[str, str] = {}
    if (ROOT / WORKFLOWS_DIR).is_dir():
        for file in sorted((ROOT / WORKFLOWS_DIR).glob("*.yml")):
            workflows[f"{WORKFLOWS_DIR}/{file.name}"] = read(
                f"{WORKFLOWS_DIR}/{file.name}"
            ) or ""
    if not workflows:
        problems.append(f"{WORKFLOWS_DIR}: нет ни одного файла конвейера")
    elif not any("cargo build" in text or "cargo zigbuild" in text for text in workflows.values()):
        # Конвейер сборки обязан существовать: без него проверка целей и флагов
        # ничего не значит, а «нет файла» не должно выглядеть успехом.
        problems.append(f"{WORKFLOWS_DIR}: нет конвейера, который собирает проект")

    # 1. Запрещённые значения — по строке и с привязкой к задаче.
    for path in [CONFIG, *CHECKED, *workflows]:
        text = read(path) if path not in workflows else workflows[path]
        if text is None:
            problems.append(f"{path}: файл не найден")
            continue
        owners = job_of_lines(text)
        for index, line in enumerate(text.split("\n")):
            stripped = line.strip()
            if stripped.startswith("#"):
                continue
            owner = owners[index] if index < len(owners) else None
            for pattern, label in FORBIDDEN:
                if not pattern.search(line):
                    continue
                # Единственное исключение: замерная задача и только она.
                if label.startswith("x86-64-v3") and owner == BENCH_JOB:
                    continue
                problems.append(f"{path}:{index + 1}: запрещено {label}")

    # 2. Цели конвейера описаны в файле, у каждой зафиксирована база.
    targets: set[str] = set()
    for text in workflows.values():
        targets.update(re.findall(r"^\s+target:\s*(\S+)\s*$", text, re.M))
    for target in sorted(targets):
        block = f"[target.{target}]"
        if block not in config:
            problems.append(f"{CONFIG}: нет блока {block} для цели конвейера")
            continue
        body = config.split(block, 1)[1].split("[target.", 1)[0]
        if "target-cpu=x86-64" not in body:
            problems.append(f"{CONFIG}: у {target} не зафиксирована базовая ISA")
    if targets and 'linker = "musl-gcc"' not in config:
        problems.append(f"{CONFIG}: для musl не задан линковщик musl-gcc")

    # 3. Конвейер не задаёт RUSTFLAGS (кроме замеров) и не задаёт rustflags.
    for path, text in workflows.items():
        owners = job_of_lines(text)
        for index, line in enumerate(text.split("\n")):
            if line.strip().startswith("#"):
                continue
            owner = owners[index] if index < len(owners) else None
            if "RUSTFLAGS" in line and owner != BENCH_JOB:
                problems.append(
                    f"{path}:{index + 1}: RUSTFLAGS вне задачи {BENCH_JOB} — "
                    "флаги должны браться из .cargo/config.toml"
                )
            if re.match(r"^\s+rustflags:", line) and owner != BENCH_JOB:
                problems.append(
                    f"{path}:{index + 1}: rustflags в матрице — единственный "
                    "источник это .cargo/config.toml"
                )

    # 4. Ускоренные пути — только через isa.rs. Иначе проверка возможностей и
    #    выключатель SR_NO_SIMD обходятся новым `#[target_feature]` в любом файле.
    sources_dir = ROOT / "src"
    if sources_dir.is_dir():
        for file in sorted(sources_dir.rglob("*.rs")):
            if file.name == "isa.rs":
                continue
            text = file.read_text(encoding="utf-8").replace("\r\n", "\n")
            for index, line in enumerate(text.split("\n"), start=1):
                if line.strip().startswith("//"):
                    continue
                if "is_x86_feature_detected" in line or re.search(
                    r"#\[target_feature", line
                ):
                    problems.append(
                        f"src/{file.name}:{index}: проверка возможностей и "
                        "#[target_feature] допустимы только в src/isa.rs"
                    )
    else:
        problems.append("src: каталог с исходниками не найден")

    if as_json:
        print(
            json.dumps(
                {"problems": problems, "targets": sorted(targets)},
                ensure_ascii=False,
                indent=2,
            )
        )
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
