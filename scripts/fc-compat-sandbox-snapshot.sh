#!/usr/bin/env bash
# Snapshot round-trip under generated deny-by-default macOS sandbox profiles.

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

tmp="$(mktemp -d /tmp/heph-fc-sandbox-snap.XXXXXX)"
rootfs="$tmp/rootfs.ext4"
snap="$tmp/fc-snap.bin"
sock_a="$tmp/fc-a.sock"
sock_b="$tmp/fc-b.sock"
log_a="$tmp/fc-a.log"
log_b="$tmp/fc-b.log"
profile_a="$tmp/sandbox-a.sb"
profile_b="$tmp/sandbox-b.sb"
server_a=""
server_b=""

cleanup() {
  kill ${server_a:-} ${server_b:-} 2>/dev/null || true
  wait ${server_a:-} ${server_b:-} 2>/dev/null || true
  rm -rf "$tmp"
}
trap cleanup EXIT

rm -f "${TMPDIR:-/tmp}/hephaestus-vsock.sock"
cp -c "$rootfs_src" "$rootfs"

make_profile() {
  local out="$1"
  scripts/generate-fc-sandbox-profile.sh \
    --output "$out" \
    --work-dir "$tmp" \
    --work-dir /tmp \
    --work-dir "${TMPDIR:-/tmp}" \
    --read "$kernel" \
    --read-write-file "$rootfs"
}

wait_sock() {
  local sock="$1" pid="$2" err="$3" profile="$4"
  for _ in $(seq 1 50); do
    [[ -S "$sock" ]] && return 0
    if ! kill -0 "$pid" 2>/dev/null; then
      echo "hephaestus-firecracker exited before creating $sock" >&2
      cat "$err" >&2 || true
      echo "--- sandbox profile ---" >&2
      cat "$profile" >&2
      exit 1
    fi
    sleep 0.1
  done
  echo "hephaestus-firecracker did not create $sock" >&2
  cat "$err" >&2 || true
  echo "--- sandbox profile ---" >&2
  cat "$profile" >&2
  exit 1
}

make_profile "$profile_a"
./build/cargo_target/debug/hephaestus-firecracker \
  --api-sock "$sock_a" \
  --id fc-sandbox-snap-a \
  --sandbox-profile "$profile_a" \
  >"$tmp/server-a.out" \
  2>"$tmp/server-a.err" &
server_a=$!
wait_sock "$sock_a" "$server_a" "$tmp/server-a.err" "$profile_a"
compat/firectl-harness/firectl-harness \
  -sock "$sock_a" -kernel "$kernel" -rootfs "$rootfs" \
  -log "$log_a" -snapshot-save "$snap" \
  -vcpu 2 -mem 512 -mem-patch 512
kill "$server_a" 2>/dev/null || true
wait "$server_a" 2>/dev/null || true
server_a=""

make_profile "$profile_b"
./build/cargo_target/debug/hephaestus-firecracker \
  --api-sock "$sock_b" \
  --id fc-sandbox-snap-b \
  --sandbox-profile "$profile_b" \
  >"$tmp/server-b.out" \
  2>"$tmp/server-b.err" &
server_b=$!
wait_sock "$sock_b" "$server_b" "$tmp/server-b.err" "$profile_b"
compat/firectl-harness/firectl-harness \
  -sock "$sock_b" -kernel "$kernel" -rootfs "$rootfs" \
  -log "$log_b" -snapshot-load "$snap" -pause \
  -vcpu 2 -mem 512 -mem-patch 512

echo "restrictive sandbox snapshot e2e passed"
