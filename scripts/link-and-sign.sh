#!/usr/bin/env bash
#
# Rust linker wrapper that invokes the real linker (`cc`) and then
# ad-hoc codesigns the produced binary with our VM entitlements.
#
# Configured via .cargo/config.toml:
#   [target.aarch64-apple-darwin]
#   linker = "scripts/link-and-sign.sh"
#
# This is the least-invasive way to get every `cargo build` to produce a
# binary capable of calling Virtualization.framework APIs — no developer
# has to remember to run `codesign` after each build.

set -euo pipefail

# Forward all arguments to the default C compiler. rustc passes the full
# clang-compatible flag set here; cc is the conventional alias.
cc "$@"

# Find the -o <output> pair so we know what to codesign. For non-binary
# link steps (intermediate objects, etc.) there may not be one, which is
# fine; we just skip signing.
out=""
prev=""
for arg in "$@"; do
    if [[ "$prev" == "-o" ]]; then
        out="$arg"
        break
    fi
    prev="$arg"
done

if [[ -z "$out" || ! -f "$out" ]]; then
    exit 0
fi

# Only sign the final hephaestus CLI binary. Everything else the linker
# produces (build scripts, proc-macro dylibs, test harnesses, dep rlibs)
# must be left alone — macOS will SIGKILL build scripts whose entitlements
# don't match the context they run in.
#
# Cargo emits the actual binary under .../deps/hephaestus-<hash> and then
# hard-links it to .../debug/hephaestus, so we have to match the hashed
# form in deps/ — that's the one the linker produces. Build scripts live at
# .../build/<crate>-<hash>/build-script-build; proc-macro dylibs are named
# lib*.dylib.
base="$(basename "$out")"
if [[ "$out" == *"/build/"*"/build-script-"* ]]; then
    exit 0
fi
if [[ "$base" == lib*.dylib ]]; then
    exit 0
fi
if [[ "$base" != "hephaestus" && "$base" != hephaestus-* ]]; then
    exit 0
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
entitlements="${script_dir}/../hephaestus.entitlements"

if [[ -f "$entitlements" ]]; then
    codesign --force --sign - --timestamp=none --entitlements "$entitlements" "$out" 2>/dev/null || true
fi
