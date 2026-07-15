#!/usr/bin/env python3
"""Validate vmnet provisioning profiles and generate app signing plists."""

from __future__ import annotations

import argparse
import datetime
import plistlib
from pathlib import Path
from typing import Any, cast

Profile = dict[str, Any]


def load_plist(path: Path) -> Profile:
    with path.open("rb") as stream:
        return cast(Profile, plistlib.load(stream))


def write_plist(path: Path, value: Profile) -> None:
    with path.open("wb") as stream:
        plistlib.dump(value, stream, sort_keys=True)


def profile_matches(
    profile: Profile,
    bundle_id: str,
    now: datetime.datetime | None = None,
) -> bool:
    entitlements = profile.get("Entitlements", {})
    app_id = entitlements.get("com.apple.application-identifier") or entitlements.get(
        "application-identifier", ""
    )
    platforms = profile.get("Platform", [])
    expires = profile.get("ExpirationDate")
    current = now or datetime.datetime.now(datetime.timezone.utc)
    if expires is not None and expires.tzinfo is None:
        expires = expires.replace(tzinfo=datetime.timezone.utc)
    return bool(
        app_id.endswith("." + bundle_id)
        and entitlements.get("com.apple.vm.networking") is True
        and "OSX" in platforms
        and (expires is None or expires > current)
    )


def signing_entitlements(profile: Profile) -> Profile:
    profile_entitlements = profile["Entitlements"]
    app_id = profile_entitlements.get(
        "com.apple.application-identifier"
    ) or profile_entitlements.get("application-identifier")
    team_id = profile_entitlements["com.apple.developer.team-identifier"]
    return {
        "com.apple.application-identifier": app_id,
        "com.apple.developer.team-identifier": team_id,
        "com.apple.security.virtualization": True,
        "com.apple.vm.networking": True,
    }


def app_info(bundle_id: str, executable: str) -> Profile:
    return {
        "CFBundleExecutable": executable,
        "CFBundleIdentifier": bundle_id,
        "CFBundleName": "Hephaestus",
        "CFBundlePackageType": "APPL",
        "CFBundleShortVersionString": "1.0",
        "CFBundleVersion": "1",
        "LSBackgroundOnly": True,
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    matches = subparsers.add_parser("matches")
    matches.add_argument("profile", type=Path)
    matches.add_argument("bundle_id")

    entitlements = subparsers.add_parser("write-entitlements")
    entitlements.add_argument("profile", type=Path)
    entitlements.add_argument("output", type=Path)

    info = subparsers.add_parser("write-info")
    info.add_argument("output", type=Path)
    info.add_argument("bundle_id")
    info.add_argument("executable")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.command == "matches":
        return 0 if profile_matches(load_plist(args.profile), args.bundle_id) else 1
    if args.command == "write-entitlements":
        write_plist(args.output, signing_entitlements(load_plist(args.profile)))
        return 0
    if args.command == "write-info":
        write_plist(args.output, app_info(args.bundle_id, args.executable))
        return 0
    raise AssertionError(f"unknown command: {args.command}")


if __name__ == "__main__":
    raise SystemExit(main())
