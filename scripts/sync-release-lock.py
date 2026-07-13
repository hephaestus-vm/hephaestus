#!/usr/bin/env python3
"""Synchronize shipped package entries in Cargo.lock with version.txt."""

from __future__ import annotations

import re
from pathlib import Path

PACKAGES = ("hephaestus-cli", "hephaestus-firecracker", "hephaestus-jailer")
LOCK_PATH = Path("Cargo.lock")


def main() -> int:
    version = Path("version.txt").read_text(encoding="utf-8").strip()
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:[.-][0-9A-Za-z.-]+)?", version):
        raise SystemExit(f"invalid version.txt: {version}")

    content = LOCK_PATH.read_text(encoding="utf-8")
    for package in PACKAGES:
        pattern = re.compile(
            rf'(\[\[package\]\]\nname = "{re.escape(package)}"\nversion = ")[^"]+("\n)'
        )
        content, count = pattern.subn(rf"\g<1>{version}\g<2>", content, count=1)
        if count != 1:
            raise SystemExit(f"expected one Cargo.lock entry for {package}, found {count}")

    LOCK_PATH.write_text(content, encoding="utf-8")
    print(f"Cargo.lock synchronized to {version}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
