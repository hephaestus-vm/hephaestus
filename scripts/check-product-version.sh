#!/usr/bin/env bash
# Keep the shipped binary, lockfile, release manifest, and changelog versions aligned.
set -euo pipefail

version="$(tr -d '[:space:]' < version.txt)"
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$ ]]; then
    echo "invalid version.txt: $version" >&2
    exit 1
fi

manifest_version="$(jq -r '.["."]' .release-please-manifest.json)"
if [[ "$manifest_version" != "$version" ]]; then
    echo "release manifest version $manifest_version does not match $version" >&2
    exit 1
fi

metadata="$(cargo metadata --locked --no-deps --format-version 1)"
for package in hephaestus-cli hephaestus-firecracker hephaestus-jailer; do
    package_version="$(jq -r --arg package "$package" \
        '.packages[] | select(.name == $package) | .version' <<<"$metadata")"
    if [[ "$package_version" != "$version" ]]; then
        echo "$package version $package_version does not match $version" >&2
        exit 1
    fi

    lock_version="$(awk -v package="$package" '
        $0 == "name = \"" package "\"" {
            getline
            gsub(/^version = \"|\"$/, "")
            print
            exit
        }
    ' Cargo.lock)"
    if [[ "$lock_version" != "$version" ]]; then
        echo "Cargo.lock $package version $lock_version does not match $version" >&2
        exit 1
    fi
done

if ! grep -Fq "## [$version]" CHANGELOG.md; then
    echo "CHANGELOG.md has no section for $version" >&2
    exit 1
fi

echo "Product version OK: $version"
