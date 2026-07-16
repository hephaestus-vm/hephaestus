#!/usr/bin/env python3
"""Canonicalize and escape paths for macOS sandbox profile forms."""

from __future__ import annotations

import argparse
from pathlib import Path


def scheme_escape(value: str) -> str:
    return value.replace("\\", "\\\\").replace('"', '\\"')


def literal_form(path: Path) -> str:
    canonical_parent = path.parent.resolve(strict=True)
    canonical = canonical_parent / path.name
    return f'(literal "{scheme_escape(str(canonical))}")'


def subpath_form(path: Path) -> str:
    path.mkdir(parents=True, exist_ok=True)
    canonical = path.resolve(strict=True)
    return f'(subpath "{scheme_escape(str(canonical))}")'


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("form", choices=("literal", "subpath"))
    parser.add_argument("path", type=Path)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.form == "literal":
        print(literal_form(args.path))
    else:
        print(subpath_form(args.path))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
