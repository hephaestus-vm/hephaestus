#!/usr/bin/env bash
# Run the Firecracker Go-SDK compat harness without booting a VM.
#
# This is CI-safe: it creates dummy kernel/rootfs files, exercises the
# config/control-plane wire surface, and passes -skip-boot so the backend never
# constructs a VZVirtualMachine or needs real guest artifacts.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

cargo build -p hephaestus-firecracker
(
  cd compat/firectl-harness
  go build -o firectl-harness .
)

tmp="$(mktemp -d /tmp/heph-fc-ci.XXXXXX)"
sock="$tmp/fc.sock"
kernel="$tmp/dummy-vmlinux"
rootfs="$tmp/dummy-rootfs.ext4"
log="$tmp/fc-compat.log"
sandbox_profile="$tmp/permissive.sb"
server=""

cleanup() {
  if [[ -n "$server" ]]; then
    kill "$server" 2>/dev/null || true
    wait "$server" 2>/dev/null || true
  fi
  rm -rf "$tmp"
}
trap cleanup EXIT

touch "$kernel" "$rootfs"
cat >"$sandbox_profile" <<'EOF'
(version 1)
(allow default)
EOF

./build/cargo_target/debug/hephaestus-firecracker \
  --api-sock "$sock" \
  --id fc-compat-config \
  --sandbox-profile "$sandbox_profile" \
  >"$tmp/server.out" \
  2>"$tmp/server.err" &
server=$!

for _ in $(seq 1 50); do
  [[ -S "$sock" ]] && break
  sleep 0.1
done

if [[ ! -S "$sock" ]]; then
  echo "hephaestus-firecracker did not create $sock" >&2
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
  exit 1
fi
