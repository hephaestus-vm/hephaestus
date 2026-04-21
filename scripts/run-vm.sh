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
if [[ "${HEPHAESTUS_TTY:-0}" == "1" ]]; then
    extra_flags+=(--tty)
fi
if [[ -n "${HEPHAESTUS_IP:-}" ]]; then
    extra_flags+=(--ip "$HEPHAESTUS_IP")
fi

id="${HEPHAESTUS_ID:-dev}"

# Each VM gets its own rootfs clone so concurrent `run`s don't fight over
# the same read-write block device. APFS `cp -c` is a CoW snapshot and is
# effectively free for a sparse file (~10 ms), so we re-create it on every
# run rather than maintaining a cache; this also means every run sees a
# clean rootfs, which is usually what a CI-ish caller wants.
if [[ "${HEPHAESTUS_ROOTFS_SHARED:-0}" != "1" ]]; then
    clone_dir="${TMPDIR:-/tmp}/hephaestus/rootfs"
    mkdir -p "$clone_dir"
    clone="$clone_dir/$id.ext4"
    rm -f "$clone"
    cp -c "$rootfs" "$clone"
    rootfs="$clone"
fi

# Similar story for the initfs — apple/container's cache is shared state
# that VZ would lock; a per-id clone keeps parallel runs isolated.
if [[ "${HEPHAESTUS_INITFS_SHARED:-0}" != "1" ]]; then
    initfs_dir="${TMPDIR:-/tmp}/hephaestus/initfs"
    mkdir -p "$initfs_dir"
    initfs_clone="$initfs_dir/$id.ext4"
    rm -f "$initfs_clone"
    cp -c "$initfs" "$initfs_clone"
    initfs="$initfs_clone"
fi

exec "$bin" run --id "$id" \
    --kernel "$kernel" \
    --initfs "$initfs" \
    --rootfs "$rootfs" \
    "${extra_flags[@]}" \
    -- "$@"
