#!/usr/bin/env python3
"""Проверка флагов сборки и границ ускорений.

Зачем. Флаги базовой ISA заданы в `.cargo/config.toml`, но переменная окружения
`RUSTFLAGS` полностью перекрывает файл. Пока значения жили в двух местах,
правка в файле в конвейере не действовала — и это ничем не обнаруживалось
(ПР-15, Д-69). Скрипт закрывает класс дефектов:

1. в конфигурации сборки нет запрещённых значений (target-cpu=native,
   x86-64-v2/v3/v4, глобальные +avx*, +fma, +sse4.*);
2. все цели, которые собирает конвейер, описаны в `.cargo/config.toml`;
3. конвейер не задаёт RUSTFLAGS и rustflags ни в одной задаче, кроме замеров;
4. проверка возможностей и ускоренные атрибуты встречаются только в src/isa.rs,
   а функция с target_feature принимает пропуск Avx2 — иначе выключатель
   SR_NO_SIMD обходится новым ускоренным путём в любом файле.

Исключение ровно одно: вариантная сборка x86-64-v3 в задаче isa-bench (числа для
сравнения, а не поставка). Задача узнаётся по имени и по содержимому.

Скрипт падает, если файл конфигурации или конвейера не найден: пропуск проверки
из-за отсутствующего файла — ложноотрицательный результат, а не успех.

Запуск:  python3 scripts/check-flags.py [--json]
Выход:   0 — всё сходится, 1 — есть нарушения.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

CONFIG = ".cargo/config.toml"
WORKFLOWS_DIR = ".github/workflows"

# Запрещённые значения: ищем подстроки, а не регулярные выражения — так нельзя
# промахнуться из-за экранирования, а ложные срабатывания отсекаются контекстом.
FORBIDDEN = [
    ("target-cpu=native", "target-cpu=native"),
    ("target-cpu = native", "target-cpu=native"),
    ("x86-64-v2", "x86-64-v2"),
    ("x86-64-v3", "x86-64-v3"),
    ("x86-64-v4", "x86-64-v4"),
    ("target-feature=+avx", "глобальный +avx*"),
    ("target-feature = +avx", "глобальный +avx*"),
    ("target-feature=+fma", "глобальный +fma"),
    ("target-feature=+sse4", "глобальный +sse4.*"),
]

CHECKED = ["Cargo.toml", "build.rs", "rust-toolchain.toml"]

BENCH_JOB = "isa-bench"
BENCH_MARK = "bench_"


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


def strip_comment(line: str) -> str:
    """Убрать хвостовой комментарий, если он не внутри кавычек."""
    quote = ""
    for index, ch in enumerate(line):
        if quote:
            if ch == quote:
                quote = ""
        elif ch in {'"', "'"}:
            quote = ch
        elif ch == "#":
            return line[:index]
    return line


def job_name(candidate: str) -> str | None:
    """Имя задачи из строки вида `  имя:` (возможны кавычки и комментарий)."""
    text = strip_comment(candidate).rstrip()
    if not text.endswith(":") or not text.startswith("  "):
        return None
    if text.startswith("   "):
        return None
    name = text[2:-1].strip()
    if name[:1] in {'"', "'"} and name[-1:] == name[:1]:
        name = name[1:-1]
    if not name or " " in name:
        return None
    return name


def job_of_lines(text: str) -> list[str | None]:
    """Для каждой строки — имя задачи (или None, если строка вне задачи).

    Задача верхнего уровня — ключ с отступом ровно в два пробела внутри `jobs:`.
    Строка без отступа закрывает раздел: например, `env:` верхнего уровня
    действует на все задачи и проверяется отдельно.
    """
    owners: list[str | None] = []
    current: str | None = None
    in_jobs = False
    for line in text.split("\n"):
        bare = strip_comment(line).rstrip()
        if bare == "jobs:":
            in_jobs = True
            current = None
            owners.append(None)
            continue
        if in_jobs:
            found = job_name(line)
            if found:
                current = found
                owners.append(current)
                continue
            if bare and not bare.startswith(" "):
                in_jobs = False
                current = None
        owners.append(current)
    return owners


def is_bench_job(text: str, owner: str | None) -> bool:
    """Задача замеров: имя совпадает и в самой задаче есть замер."""
    if owner != BENCH_JOB:
        return False
    owners = job_of_lines(text)
    block = [
        line
        for line, name in zip(text.split("\n"), owners, strict=False)
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
        lines = file.read_text(encoding="utf-8").replace("\r\n", "\n").split("\n")
        for index, line in enumerate(lines, start=1):
            stripped = line.strip()
            if stripped.startswith("//"):
                continue
            in_isa = rel == "src/isa.rs"
            if not in_isa and (
                "is_x86_feature_detected" in line
                or "target_feature" in line
                or "std::arch" in line
                or "cfg!(target_feature" in line
                or "#[cfg(target_feature" in line
            ):
                problems.append(
                    f"{rel}:{index}: проверка возможностей и target_feature "
                    "допустимы только в src/isa.rs"
                )
                continue
            # Внутри isa.rs: ускоренная функция обязана принимать пропуск Avx2.
            # Атрибут и подпись могут занимать несколько строк, а enable="avx2"
            # пишут и без пробелов — поэтому берём текст до открывающей скобки.
            if in_isa and "#[target_feature" in line:
                tail = "\n".join(lines[index - 1 : index + 12])
                signature = tail.split("{", 1)[0]
                if "fn " in signature and "Avx2" not in signature:
                    problems.append(
                        f"{rel}:{index}: функция с target_feature не принимает "
                        "пропуск Avx2 — ускоренный путь обойдёт SR_NO_SIMD"
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
        # целей и шаг, который действительно запускает сборку.
        builds = []
        for path, text in workflows.items():
            has_target = any(
                strip_comment(line).strip().startswith("target:")
                for line in text.split("\n")
            )
            has_build = any(
                strip_comment(line).strip().startswith("cargo build")
                or strip_comment(line).strip().startswith("cargo zigbuild")
                or strip_comment(line).strip().startswith("run: cargo build")
                or strip_comment(line).strip().startswith("run: cargo zigbuild")
                for line in text.split("\n")
            )
            if has_target and has_build:
                builds.append(path)
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
            for needle, label in FORBIDDEN:
                if needle not in line:
                    continue
                if label.startswith("x86-64-v3") and is_bench_job(text, owner):
                    continue
                problems.append(f"{path}:{index + 1}: запрещено {label}")

    # 2. Цели конвейера описаны в файле, у каждой зафиксирована база.
    targets: set[str] = set()
    for text in workflows.values():
        for line in text.split("\n"):
            bare = strip_comment(line).strip()
            if bare.startswith("target:"):
                value = bare[len("target:") :].strip().strip("\"'")
                if value:
                    targets.add(value)
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
            bench = is_bench_job(text, owner)
            if "RUSTFLAGS" in line and not bench:
                problems.append(
                    f"{path}:{index + 1}: RUSTFLAGS вне задачи {BENCH_JOB} — "
                    "флаги должны браться из .cargo/config.toml"
                )
            if line.strip().startswith("rustflags:") and not bench:
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
