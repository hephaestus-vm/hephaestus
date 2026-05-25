#!/usr/bin/env bash
# CI-safe restrictive-sandbox compat smoke.
#
# Generates a deny-by-default macOS sandbox profile for a config-only
# hephaestus-firecracker process, proves an unrelated file is denied, then runs
# the Go SDK compat harness with -skip-boot.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

cargo build -p hephaestus-firecracker
(
  cd compat/firectl-harness
  go build -o firectl-harness .
)

tmp="$(mktemp -d /tmp/heph-fc-sandbox.XXXXXX)"
deny_dir="$(mktemp -d /tmp/heph-fc-deny.XXXXXX)"
sock="$tmp/fc.sock"
kernel="$tmp/dummy-vmlinux"
rootfs="$tmp/dummy-rootfs.ext4"
log="$tmp/fc-compat.log"
profile="$tmp/restrictive.sb"
deny_probe="$deny_dir/not-allowed.txt"
server=""

cleanup() {
  if [[ -n "$server" ]]; then
    kill "$server" 2>/dev/null || true
    wait "$server" 2>/dev/null || true
  fi
  rm -rf "$tmp" "$deny_dir"
}
trap cleanup EXIT

touch "$kernel" "$rootfs"
echo "sandbox should deny this" >"$deny_probe"

scripts/generate-fc-sandbox-profile.sh \
  --output "$profile" \
  --work-dir "$tmp" \
  --read "$kernel" \
  --read "$rootfs"

./build/cargo_target/debug/hephaestus-firecracker \
  --api-sock "$sock" \
  --id fc-compat-sandbox-config \
  --sandbox-profile "$profile" \
  --sandbox-deny-probe "$deny_probe" \
  >"$tmp/server.out" \
  2>"$tmp/server.err" &
server=$!

for _ in $(seq 1 50); do
  [[ -S "$sock" ]] && break
  if ! kill -0 "$server" 2>/dev/null; then
    echo "hephaestus-firecracker exited before creating $sock" >&2
    cat "$tmp/server.err" >&2 || true
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

if ! grep -q "sandbox deny probe blocked" "$tmp/server.err"; then
  echo "sandbox deny probe did not report a block" >&2
  cat "$tmp/server.err" >&2 || true
  exit 1
fi

if ! compat/firectl-harness/firectl-harness \
  -sock "$sock" \
  -kernel "$kernel" \
  -rootfs "$rootfs" \
  -log "$log" \
  -skip-boot; then
  cat "$tmp/server.err" >&2 || true
  echo "--- sandbox profile ---" >&2
  cat "$profile" >&2
  exit 1
fi

echo "restrictive sandbox config-only e2e passed"
