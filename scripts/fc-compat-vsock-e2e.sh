#!/usr/bin/env bash
# Real-VM, headless e2e for the Firecracker /vsock UDS bridge and
# guest-visible MMDS vsock service.
#
# Requires real apple/container kernel/rootfs artifacts. This is intentionally
# not wired into GitHub-hosted CI; use `just fc-compat-vsock-e2e` on a Mac that
# can run VZ VMs.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
vsock_tool="$repo_root/scripts/fc_compat_vsock_e2e.py"
cd "$repo_root"

cargo build -p hephaestus-firecracker
scripts/build-agent.sh
(
  cd compat/firectl-harness
  go build -o firectl-harness .
)

cdir="$HOME/Library/Application Support/com.apple.container"
kernel="$(find "$cdir/kernels" -maxdepth 1 -type f -name 'vmlinux-*' -print -quit 2>/dev/null || true)"
snaps=("$cdir"/snapshots/*/snapshot)
if [[ -z "$kernel" ]] || [[ ! -e "${snaps[0]:-}" ]]; then
  echo "no artifacts; run: just artifacts" >&2
  exit 1
fi
rootfs_src=$(stat -f '%z %N' "${snaps[@]}" | sort -nr | head -1 | cut -d' ' -f2-)

tmp="$(mktemp -d /tmp/heph-vsock-e2e.XXXXXX)"
sock="$tmp/fc.sock"
vsock="$tmp/guest-vsock.sock"
rootfs="$tmp/rootfs.ext4"
log="$tmp/fc.log"
serial="$tmp/serial.log"
profile="$tmp/restrictive-vsock.sb"
server=""
cleanup() {
  if [[ -n "$server" ]]; then
    kill "$server" 2>/dev/null || true
    wait "$server" 2>/dev/null || true
  fi
  rm -rf "$tmp"
}
trap cleanup EXIT

cp -c "$rootfs_src" "$rootfs"

server_args=(
  --api-sock "$sock"
  --id fc-vsock-e2e
)
if [[ "${HEPHAESTUS_SANDBOX:-0}" == "1" ]]; then
  scripts/generate-fc-sandbox-profile.sh \
    --output "$profile" \
    --work-dir "$tmp" \
    --work-dir /tmp \
    --work-dir "${TMPDIR:-/tmp}" \
    --read "$kernel" \
    --read "$repo_root/build/agent.cpio.gz" \
    --read-write-file "$rootfs"
  server_args+=(--sandbox-profile "$profile")
fi

./build/cargo_target/debug/hephaestus-firecracker \
  "${server_args[@]}" \
  >"$tmp/server.out" \
  2>"$tmp/server.err" &
server=$!
for _ in $(seq 1 50); do [[ -S "$sock" ]] && break; sleep 0.1; done
if [[ ! -S "$sock" ]]; then
  cat "$tmp/server.err" >&2 || true
  exit 1
fi

api() {
  local method="$1" path="$2" body="${3:-}"
  if [[ -n "$body" ]]; then
    curl -fsS --unix-socket "$sock" -X "$method" \
      -H 'content-type: application/json' \
      --data "$body" \
      "http://localhost$path" >/dev/null
  else
    curl -fsS --unix-socket "$sock" -X "$method" "http://localhost$path" >/dev/null
  fi
}

api PUT /logger "$(python3 "$vsock_tool" logger-config "$log")"
api PUT /metrics "$(python3 "$vsock_tool" metrics-config "$log.metrics")"
api PUT /machine-config '{"vcpu_count":2,"mem_size_mib":512}'
api PUT /mmds/config '{"network_interfaces":[],"version":"V2","ipv4_address":"169.254.169.254"}'
api PUT /mmds '{"latest":{"meta-data":{"instance-id":"i-hephaestus-vsock-e2e"}}}'
api PUT /vsock "$(python3 "$vsock_tool" vsock-config "$vsock")"
api PUT /boot-source "$(python3 "$vsock_tool" boot-config \
  "$kernel" "$repo_root/build/agent.cpio.gz")"
api PUT /drives/rootfs "$(python3 "$vsock_tool" drive-config "$rootfs")"
api PUT /actions '{"action_type":"InstanceStart"}'

for _ in $(seq 1 100); do [[ -S "$vsock" ]] && break; sleep 0.1; done
if [[ ! -S "$vsock" ]]; then
  echo "vsock bridge did not create $vsock" >&2
  cat "$tmp/server.err" >&2 || true
  exit 1
fi

python3 "$vsock_tool" check-guest "$vsock"

echo "serial log: $serial"
echo "server log:  $tmp/server.err"
