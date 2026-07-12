#!/usr/bin/env python3
"""Check relative links in the repository's maintained Markdown documents."""

from __future__ import annotations

import re
import sys
from pathlib import Path
from urllib.parse import unquote

ROOT = Path(__file__).resolve().parent.parent
LINK = re.compile(r"(?<!!)\[[^\]]*\]\(([^)]+)\)")
SKIP_PREFIXES = ("http://", "https://", "mailto:", "#")
ROOT_DOCS = {"README.md", "CONTRIBUTING.md", "SECURITY.md", "CHANGELOG.md"}


def markdown_files() -> list[Path]:
    files = [ROOT / name for name in sorted(ROOT_DOCS) if (ROOT / name).exists()]
    files.extend(sorted((ROOT / "docs").rglob("*.md")))
    return files


def target_path(source: Path, raw_target: str) -> Path | None:
    # Markdown permits an optional title after a whitespace separator. Project
    # paths do not contain spaces, so the first token is the link destination.
    target = raw_target.strip().lstrip("<").split()[0].rstrip(">")
    if target.startswith(SKIP_PREFIXES):
        return None
    target = unquote(target.split("#", 1)[0])
    if not target:
        return None
    if target.startswith("/"):
        return ROOT / target.lstrip("/")
    return source.parent / target


def main() -> int:
    failures: list[str] = []
    for source in markdown_files():
        text = source.read_text(encoding="utf-8")
        for match in LINK.finditer(text):
            destination = target_path(source, match.group(1))
            if destination is None or destination.exists():
                continue
            line = text.count("\n", 0, match.start()) + 1
            failures.append(
                f"{source.relative_to(ROOT)}:{line}: missing {match.group(1)}"
            )

    if failures:
        print("Documentation link check failed:", file=sys.stderr)
        print("\n".join(failures), file=sys.stderr)
        return 1
    print(f"Documentation links OK ({len(markdown_files())} files)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
