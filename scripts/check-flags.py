#!/usr/bin/env python3
"""Проверка флагов сборки и границ ускорений.

Зачем. Флаги базовой ISA заданы в `.cargo/config.toml`, но переменная окружения
`RUSTFLAGS` полностью перекрывает файл. Пока значения жили в двух местах,
правка в файле в конвейере не действовала — и это ничем не обнаруживалось
(ПР-15, Д-69). Скрипт закрывает класс дефектов:

1. в конфигурации сборки нет запрещённых значений (`target-cpu=native`,
   `x86-64-v2/v3/v4`, глобальные `+avx*`, `+fma`, `+sse4.*`);
2. все цели, которые собирает конвейер, описаны в `.cargo/config.toml`;
3. конвейер не задаёт `RUSTFLAGS` и `rustflags` ни в одной задаче, кроме замеров;
4. проверка возможностей и `#[target_feature]` встречаются только в `src/isa.rs`,
   а функция с `#[target_feature(enable = "avx2")]` обязана принимать пропуск
   `Avx2` — иначе выключатель `SR_NO_SIMD` обходится новым ускоренным путём.

Исключение ровно одно: вариантная сборка `x86-64-v3` в задаче `isa-bench`
(числа для сравнения, а не поставка). Задача узнаётся по имени и по содержимому.

Скрипт падает, если файл конфигурации или конвейера не найден: пропуск проверки
из-за отсутствующего файла — ложноотрицательный результат, а не успех.

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

# Значения, запрещённые политикой (docs/PLATFORMS.md §2). Границы слов здесь не
# годятся: `+avx512f` должен ловиться так же, как `+avx`.
FORBIDDEN = [
    (re.compile(r"target-cpu\s*=\s*native"), "target-cpu=native"),
    (re.compile(r"x86-64-v2"), "x86-64-v2"),
    (re.compile(r"x86-64-v3"), "x86-64-v3"),
    (re.compile(r"x86-64-v4"), "x86-64-v4"),
    (re.compile(r"target-feature=\+avx"), "глобальный +avx*"),
    (re.compile(r"target-feature=\+fma"), "глобальный +fma"),
    (re.compile(r"target-feature=\+sse4"), "глобальный +sse4.*"),
]

CHECKED = ["Cargo.toml", "build.rs", "rust-toolchain.toml"]

# Задача замеров: ей вариантная сборка разрешена.
BENCH_JOB = "isa-bench"
BENCH_MARK = "bench_"

# Ключ задачи верхнего уровня: допускаем кавычки и хвостовой комментарий.
JOB_KEY = re.compile(r'^  "?([A-Za-z0-9_-]+)"?:[ \t]*(?:#.*)?$')


def read(path: str) -> str | None:
    file = ROOT / path
    if not file.is_file():
        return None
    return file.read_text(encoding="utf-8").replace("\r\n", "\n")


def workflow_files() -> dict[str, str]:
    """Все файлы конвейеров: GitHub принимает и .yml, и .yaml."""
    result: dict[str, str] = {}
    folder = ROOT / WORKFLOWS_DIR
    if not folder.is_dir():
        return result
    for file in sorted(folder.iterdir()):
        if file.suffix not in {".yml", ".yaml"}:
            continue
        rel = f"{WORKFLOWS_DIR}/{file.name}"
        result[rel] = read(rel) or ""
    return result


def job_of_lines(text: str) -> list[str | None]:
    """Для каждой строки — имя задачи (или None, если строка вне задачи).

    Задача верхнего уровня в GitHub Actions — ключ с отступом в два пробела
    внутри `jobs:`. Любая строка без отступа (например, `env:` верхнего уровня)
    считается «вне задачи» и проверяется отдельно: она действует на все шаги.
    """
    owners: list[str | None] = []
    current: str | None = None
    in_jobs = False
    for line in text.split("\n"):
        if line.rstrip() == "jobs:":
            in_jobs = True
            current = None
            owners.append(None)
            continue
        if in_jobs:
            match = JOB_KEY.match(line)
            if match:
                current = match.group(1)
                owners.append(current)
                continue
            # Строка без отступа — конец раздела задач (permissions, env и пр.).
            if line and not line.startswith(" "):
                in_jobs = False
                current = None
        owners.append(current)
    return owners


def is_bench_job(text: str, owner: str | None, index: int) -> bool:
    """Задача замеров: имя совпадает и в самой задаче есть замер."""
    if owner != BENCH_JOB:
        return False
    owners = job_of_lines(text)
    lines = text.split("\n")
    block = [
        line
        for line, name in zip(lines, owners, strict=False)
        if name == BENCH_JOB
    ]
    return BENCH_MARK in "\n".join(block)


def check_sources(problems: list[str]) -> None:
    sources = ROOT / "src"
    if not sources.is_dir():
        problems.append("src: каталог с исходниками не найден")
        return
    for file in sorted(sources.rglob("*.rs")):
        rel = file.relative_to(ROOT).as_posix()
        text = file.read_text(encoding="utf-8").replace("\r\n", "\n")
        lines = text.split("\n")
        for index, line in enumerate(lines, start=1):
            stripped = line.strip()
            if stripped.startswith("//"):
                continue
            if rel != "src/isa.rs" and (
                "is_x86_feature_detected" in line
                or re.search(r"#\[target_feature", line)
                or "cfg!(target_feature" in line
            ):
                problems.append(
                    f"{rel}:{index}: проверка возможностей и #[target_feature] "
                    "допустимы только в src/isa.rs"
                )
            # Внутри isa.rs: ускоренная функция обязана принимать пропуск Avx2.
            if rel == "src/isa.rs" and re.search(
                r"#\[target_feature\(enable = \"(avx2|avx|sse4[^)]*)\"\)\]", line
            ):
                signature = ""
                for follow in lines[index : index + 3]:
                    if "fn " in follow:
                        signature = follow
                        break
                if "Avx2" not in signature:
                    problems.append(
                        f"{rel}:{index}: функция с #[target_feature] не принимает "
                        "пропуск Avx2 — ускоренный путь обойдёт SR_NO_SIMD"
                    )
            if "std::arch" in line and rel != "src/isa.rs":
                problems.append(
                    f"{rel}:{index}: std::arch вне src/isa.rs — интринсики без "
                    "проверки возможностей"
                )


def main() -> int:
    as_json = "--json" in sys.argv
    problems: list[str] = []

    config = read(CONFIG)
    if config is None:
        problems.append(f"{CONFIG}: файл не найден — проверять нечего")
        config = ""

    workflows = workflow_files()
    if not workflows:
        problems.append(f"{WORKFLOWS_DIR}: нет ни одного файла конвейера")
    else:
        # Конвейер сборки обязан существовать: нужен файл, где есть матрица
        # целей и шаг cargo build. Простое упоминание «cargo build» в другом
        # конвейере за сборку не считается.
        builds = [
            path
            for path, text in workflows.items()
            if re.search(r"^\s+target:\s*", text, re.M)
            and re.search(r"cargo (build|zigbuild)", text)
        ]
        if not builds:
            problems.append(
                f"{WORKFLOWS_DIR}: нет конвейера, который собирает проект "
                "(нужна матрица целей и шаг cargo build)"
            )

    # 1. Запрещённые значения — по строке, с привязкой к задаче.
    for path in [CONFIG, *CHECKED, *workflows]:
        text = read(path) if path not in workflows else workflows[path]
        if text is None:
            problems.append(f"{path}: файл не найден")
            continue
        owners = job_of_lines(text)
        for index, line in enumerate(text.split("\n")):
            if line.strip().startswith("#"):
                continue
            owner = owners[index] if index < len(owners) else None
            for pattern, label in FORBIDDEN:
                if not pattern.search(line):
                    continue
                if label.startswith("x86-64-v3") and is_bench_job(text, owner, index):
                    continue
                problems.append(f"{path}:{index + 1}: запрещено {label}")

    # 2. Цели конвейера описаны в файле, у каждой зафиксирована база.
    targets: set[str] = set()
    for text in workflows.values():
        targets.update(
            match.strip("\"'")
            for match in re.findall(r"^\s+target:\s*(\S+)\s*$", text, re.M)
        )
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

    # 3. Конвейер не задаёт RUSTFLAGS и не переопределяет rustflags.
    for path, text in workflows.items():
        owners = job_of_lines(text)
        for index, line in enumerate(text.split("\n")):
            if line.strip().startswith("#"):
                continue
            owner = owners[index] if index < len(owners) else None
            bench = is_bench_job(text, owner, index)
            if "RUSTFLAGS" in line and not bench:
                problems.append(
                    f"{path}:{index + 1}: RUSTFLAGS вне задачи {BENCH_JOB} — "
                    "флаги должны браться из .cargo/config.toml"
                )
            if re.match(r"^\s+rustflags:", line) and not bench:
                problems.append(
                    f"{path}:{index + 1}: rustflags в матрице — единственный "
                    "источник это .cargo/config.toml"
                )

    # 4. Ускоренные пути — только через isa.rs с пропуском.
    check_sources(problems)

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
