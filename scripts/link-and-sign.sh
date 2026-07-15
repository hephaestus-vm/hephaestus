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
# Cargo emits the actual binary under .../deps/<crate>-<hash> and then
# hard-links it to .../debug/<binary-name>, so we have to match the hashed
# form in deps/ — that's the one the linker produces. Build scripts live at
# .../build/<crate>-<hash>/build-script-build; proc-macro dylibs are named
# lib*.dylib. Cargo normalizes '-' to '_' in the deps/ filename for
# multi-word crate names, so match both `hephaestus-*` and `hephaestus_*`.
base="$(basename "$out")"
if [[ "$out" == *"/build/"*"/build-script-"* ]]; then
    exit 0
fi
if [[ "$base" == lib*.dylib ]]; then
    exit 0
fi
if [[ "$base" != "hephaestus" && "$base" != hephaestus-* && "$base" != hephaestus_* ]]; then
    exit 0
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Default: base virtualization entitlement, ad-hoc signature. Both remain
# overridable for non-restricted development signatures:
#   HEPHAESTUS_ENTITLEMENTS   path to an .entitlements plist (default: base)
#   HEPHAESTUS_SIGN_IDENTITY  codesign identity (default: - for ad-hoc)
#
# A restricted entitlement also needs an embedded provisioning profile, which a
# standalone Mach-O cannot carry. Use scripts/package-vmnet-app.sh (or
# `just sign-vmnet`) for com.apple.vm.networking builds.
entitlements="${HEPHAESTUS_ENTITLEMENTS:-${script_dir}/../hephaestus.entitlements}"
sign_identity="${HEPHAESTUS_SIGN_IDENTITY:--}"

if [[ -f "$entitlements" ]]; then
    codesign --force --sign "$sign_identity" --timestamp=none \
        --entitlements "$entitlements" "$out" 2>/dev/null || true
fi
