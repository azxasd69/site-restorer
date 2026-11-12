"""Перечень состава сборки (SBOM) в формате CycloneDX 1.5.

Без внешних зависимостей: читает Cargo.lock и печатает JSON в stdout.
Так задача в конвейере не зависит от установки cargo-cyclonedx и работает
одинаково на любой машине с Python 3.

Использование: python3 tools/sbom.py > sbom.cdx.json
"""
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LOCK = ROOT / "Cargo.lock"
CARGO = ROOT / "Cargo.toml"

NAME = re.search(r'^name\s*=\s*"([^"]+)"', CARGO.read_text(encoding="utf-8"), re.M).group(1)
VERSION = re.search(r'^version\s*=\s*"([^"]+)"', CARGO.read_text(encoding="utf-8"), re.M).group(1)


def quoted(line: str) -> str:
    """Значение в кавычках из строки вида `version = "3.5.2"`."""
    parts = line.split('"')
    return parts[1] if len(parts) > 2 else ""


components = []
block = {}
for raw_line in LOCK.read_text(encoding="utf-8").splitlines():
    line = raw_line.strip()
    if line == "[[package]]":
        if block:
            components.append(block)
        block = {}
    elif line.startswith("name = "):
        block["name"] = quoted(line)
    elif line.startswith("version = "):
        block["version"] = quoted(line)
    elif line.startswith("checksum = "):
        block["checksum"] = quoted(line)
if block:
    components.append(block)

out = []
for item in components:
    if not item.get("name") or not item.get("version") or item["name"] == NAME:
        continue
    entry = {
        "type": "library",
        "name": item["name"],
        "version": item["version"],
        "purl": f"pkg:cargo/{item['name']}@{item['version']}",
    }
    # Контрольная сумма Cargo.lock — sha256 в hex, CycloneDX понимает такой вид.
    if item.get("checksum"):
        entry["hashes"] = [{"alg": "SHA-256", "content": item["checksum"]}]
    out.append(entry)

sbom = {
    "bomFormat": "CycloneDX",
    "specVersion": "1.5",
    "version": 1,
    "metadata": {"component": {"type": "application", "name": NAME, "version": VERSION}},
    "components": sorted(out, key=lambda c: (c["name"], c["version"])),
}

json.dump(sbom, sys.stdout, ensure_ascii=False, indent=2)
sys.stdout.write("\n")
print(f"компонентов: {len(out)}", file=sys.stderr)
