#!/usr/bin/env bash
# Real-VM restrictive-sandbox Firecracker compat smoke.
#
# Requires apple/container kernel/rootfs artifacts. This is the next jailer
# milestone after config-only: run the normal Go SDK compat boot/pause smoke
# with hephaestus-firecracker inside a generated deny-by-default sandbox.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

cargo build -p hephaestus-firecracker
(
  cd compat/firectl-harness
  go build -o firectl-harness .
)

cdir="$HOME/Library/Application Support/com.apple.container"
kernel="$(ls "$cdir"/kernels/vmlinux-* 2>/dev/null | head -1 || true)"
snaps=("$cdir"/snapshots/*/snapshot)
if [[ -z "$kernel" ]] || [[ ! -e "${snaps[0]:-}" ]]; then
  echo "no artifacts; run: just artifacts" >&2
  exit 1
fi
rootfs_src=$(stat -f '%z %N' "${snaps[@]}" | sort -nr | head -1 | cut -d' ' -f2-)

tmp="$(mktemp -d /tmp/heph-fc-sandbox-boot.XXXXXX)"
sock="$tmp/fc.sock"
rootfs="$tmp/rootfs.ext4"
log="$tmp/fc-compat.log"
profile="$tmp/restrictive-boot.sb"
server=""

cleanup() {
  if [[ -n "$server" ]]; then
    kill "$server" 2>/dev/null || true
    wait "$server" 2>/dev/null || true
  fi
  rm -rf "$tmp"
}
trap cleanup EXIT

rm -f "${TMPDIR:-/tmp}/hephaestus-vsock.sock"
cp -c "$rootfs_src" "$rootfs"

# VZ writes the per-instance serial log to /tmp/hephaestus-firecracker-$id.log
# today, so allow /tmp for this first real-VM sandbox milestone. The full jailer
# should move that log under the per-VM work dir and remove this broader grant.
scripts/generate-fc-sandbox-profile.sh \
  --output "$profile" \
  --work-dir "$tmp" \
  --work-dir /tmp \
  --work-dir "${TMPDIR:-/tmp}" \
  --read "$kernel" \
  --read-write-file "$rootfs"

./build/cargo_target/debug/hephaestus-firecracker \
  --api-sock "$sock" \
  --id fc-compat-sandbox \
  --sandbox-profile "$profile" \
  >"$tmp/server.out" \
  2>"$tmp/server.err" &
server=$!

for _ in $(seq 1 50); do
  [[ -S "$sock" ]] && break
  if ! kill -0 "$server" 2>/dev/null; then
    echo "hephaestus-firecracker exited before creating $sock" >&2
    cat "$tmp/server.err" >&2 || true
    echo "--- sandbox profile ---" >&2
    cat "$profile" >&2
    exit 1
  fi
  sleep 0.1
done

if [[ ! -S "$sock" ]]; then
  echo "hephaestus-firecracker did not create $sock" >&2
  cat "$tmp/server.err" >&2 || true
  echo "--- sandbox profile ---" >&2
  cat "$profile" >&2
  exit 1
fi

if ! compat/firectl-harness/firectl-harness \
  -sock "$sock" \
  -kernel "$kernel" \
  -rootfs "$rootfs" \
  -log "$log" \
  -pause; then
  cat "$tmp/server.err" >&2 || true
  echo "--- sandbox profile ---" >&2
  cat "$profile" >&2
  exit 1
fi

echo "restrictive sandbox real-VM compat e2e passed"
