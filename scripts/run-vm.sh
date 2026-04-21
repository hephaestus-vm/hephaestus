#!/usr/bin/env bash
#
# Shared helper: discover the apple/container-cached VM artifacts, then
# invoke `hephaestus run` with the remaining argv as the guest command.
#
# Example:
#   scripts/run-vm.sh /bin/echo hello
#   scripts/run-vm.sh /bin/sh -c 'uname -a; ls /'

set -euo pipefail

cdir="$HOME/Library/Application Support/com.apple.container"

kernel="$(ls "$cdir"/kernels/vmlinux-* 2>/dev/null | head -1 || true)"
snaps=("$cdir"/snapshots/*/snapshot)

if [[ -z "$kernel" ]] || [[ ! -e "${snaps[0]:-}" ]]; then
    echo "no apple/container artifacts found under:" >&2
    echo "  $cdir" >&2
    echo "seed them once with:" >&2
    echo "  container system start" >&2
    echo "  container run --rm docker.io/library/alpine:3.20 echo hi" >&2
    exit 1
fi

# Two snapshots: smaller non-sparse one is vminit initfs; larger sparse
# one is the container rootfs.
initfs=$(stat -f '%z %N' "${snaps[@]}" | sort -n  | head -1 | cut -d' ' -f2-)
rootfs=$(stat -f '%z %N' "${snaps[@]}" | sort -nr | head -1 | cut -d' ' -f2-)

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
bin="${script_dir}/../build/cargo_target/debug/hephaestus"

extra_flags=()
if [[ "${HEPHAESTUS_NETWORK:-0}" == "1" ]]; then
    extra_flags+=(--network)
fi

exec "$bin" run --id dev \
    --kernel "$kernel" \
    --initfs "$initfs" \
    --rootfs "$rootfs" \
    "${extra_flags[@]}" \
    -- "$@"
