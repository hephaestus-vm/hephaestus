#!/usr/bin/env bash
# Pool restore under a generated deny-by-default macOS sandbox profile.
# Usage: fc-compat-sandbox-pool.sh [agent|stock]

set -euo pipefail
flavor="${1:-agent}"
case "$flavor" in agent|stock) ;; *) echo "usage: $0 [agent|stock]" >&2; exit 64 ;; esac

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

cargo build -p hephaestus-cli -p hephaestus-firecracker
scripts/build-agent.sh
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

tmp="$(mktemp -d /tmp/heph-fc-sandbox-pool.XXXXXX)"
pool="$tmp/pool"
sock="$tmp/fc.sock"
log="$tmp/fc-pool.log"
profile="$tmp/restrictive-pool.sb"
server=""

cleanup() {
  if [[ -n "$server" ]]; then
    kill "$server" 2>/dev/null || true
    wait "$server" 2>/dev/null || true
  fi
  ./build/cargo_target/debug/hephaestus pool destroy --dir "$pool" 2>/dev/null || true
  rm -rf "$tmp"
}
trap cleanup EXIT

rm -f "${TMPDIR:-/tmp}/hephaestus-vsock.sock"
if [[ "$flavor" == "stock" ]]; then
  ./build/cargo_target/debug/hephaestus pool init --dir "$pool" --kernel "$kernel" \
    --rootfs "$rootfs_src" --size 1 --stock-init --settle-seconds 3
  skip_vsock=(-skip-vsock)
  id="fc-sandbox-pool-stock"
else
  ./build/cargo_target/debug/hephaestus pool init --dir "$pool" --kernel "$kernel" \
    --rootfs "$rootfs_src" --size 1
  skip_vsock=()
  id="fc-sandbox-pool"
fi

scripts/generate-fc-sandbox-profile.sh \
  --output "$profile" \
  --work-dir "$tmp" \
  --work-dir "$pool" \
  --work-dir /tmp \
  --work-dir "${TMPDIR:-/tmp}" \
  --read "$kernel"

./build/cargo_target/debug/hephaestus-firecracker \
  --api-sock "$sock" \
  --id "$id" \
  --pool-dir "$pool" \
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

compat/firectl-harness/firectl-harness \
  -sock "$sock" -kernel "$kernel" -rootfs "$pool/pristine.ext4" \
  -log "$log" -pause "${skip_vsock[@]}" \
  -vcpu 2 -mem 512 -mem-patch 512

echo "restrictive sandbox $flavor pool e2e passed"
