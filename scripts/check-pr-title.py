#!/usr/bin/env python3
"""Validate squash-merge titles as Conventional Commits."""

from __future__ import annotations

import re
import sys

ALLOWED_TYPES = (
    "build",
    "chore",
    "ci",
    "docs",
    "feat",
    "fix",
    "perf",
    "refactor",
    "revert",
    "test",
)
PATTERN = re.compile(
    rf"^({'|'.join(ALLOWED_TYPES)})(\([a-z0-9][a-z0-9._/-]*\))?!?: .+$"
)


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: check-pr-title.py '<title>'", file=sys.stderr)
        return 2
    title = sys.argv[1]
    if PATTERN.fullmatch(title):
        print(f"Conventional PR title OK: {title}")
        return 0
    print(
        "PR title must use Conventional Commit form, for example "
        "`feat(cli): add nested help`.",
        file=sys.stderr,
    )
    print(f"Received: {title}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
