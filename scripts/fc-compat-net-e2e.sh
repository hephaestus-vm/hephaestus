#!/usr/bin/env bash
# Real-VM, headless e2e for guest networking on the Firecracker HTTP API
# path. Configures PUT /network-interfaces, boots, then asks the guest agent
# (over the /vsock CONNECT 1234 bridge) whether a non-loopback network device
# is present — i.e. whether the selected VZ network attachment reached the guest.
#
# We check for the device via sysfs rather than `ip`/DHCP so the smoke does
# not depend on the rootfs shipping iproute2 or a DHCP client: attaching the
# NIC is the VMM's job; configuring L3 is the guest image's.
#
# Requires real apple/container kernel/rootfs artifacts. Not wired into
# GitHub CI; run `just fc-compat-net-e2e` on a Mac that can run VZ VMs.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
net_tool="$repo_root/scripts/fc_compat_net_e2e.py"
cd "$repo_root"

cargo build -p hephaestus-firecracker
scripts/build-agent.sh

firecracker="${HEPHAESTUS_FIRECRACKER_BIN:-./build/cargo_target/debug/hephaestus-firecracker}"
read -r -a firecracker_args <<< "${HEPHAESTUS_FIRECRACKER_ARGS:-}"
network_label="${HEPHAESTUS_NETWORK_LABEL:-VZ NAT}"

cdir="$HOME/Library/Application Support/com.apple.container"
kernel="$(find "$cdir/kernels" -maxdepth 1 -type f -name 'vmlinux-*' -print -quit 2>/dev/null || true)"
snaps=("$cdir"/snapshots/*/snapshot)
if [[ -z "$kernel" ]] || [[ ! -e "${snaps[0]:-}" ]]; then
  echo "no artifacts; run: just artifacts" >&2
  exit 1
fi
rootfs_src=$(stat -f '%z %N' "${snaps[@]}" | sort -nr | head -1 | cut -d' ' -f2-)

tmp="$(mktemp -d /tmp/heph-net-e2e.XXXXXX)"
sock="$tmp/fc.sock"
vsock="$tmp/guest-vsock.sock"
rootfs="$tmp/rootfs.ext4"
server=""
cleanup() {
  if [[ -n "$server" ]]; then
    kill "$server" 2>/dev/null || true
    wait "$server" 2>/dev/null || true
  fi
  if [[ "${HEPHAESTUS_KEEP_TMP:-0}" == 1 ]]; then
    echo "kept e2e directory: $tmp" >&2
  else
    rm -rf "$tmp"
  fi
}
trap cleanup EXIT

cp -c "$rootfs_src" "$rootfs"

"$firecracker" "${firecracker_args[@]}" \
  --api-sock "$sock" \
  --id fc-net-e2e \
  >"$tmp/server.out" \
  2>"$tmp/server.err" &
server=$!
for _ in $(seq 1 50); do
  [[ -S "$sock" ]] && break
  kill -0 "$server" 2>/dev/null || { echo "server exited early" >&2; cat "$tmp/server.err" >&2; exit 1; }
  sleep 0.1
done
if [[ ! -S "$sock" ]]; then
  cat "$tmp/server.err" >&2 || true
  exit 1
fi

api() {
  local body="$tmp/api-response" status
  status="$(curl -sS -o "$body" -w '%{http_code}' --unix-socket "$sock" -X "$1" \
    -H 'content-type: application/json' \
    ${3:+--data "$3"} \
    "http://localhost$2")"
  if [[ ! "$status" =~ ^2 ]]; then
    echo "API $1 $2 failed with HTTP $status: $(cat "$body")" >&2
    return 1
  fi
}

api PUT /machine-config '{"vcpu_count":2,"mem_size_mib":512}'
api PUT /vsock "$(python3 "$net_tool" vsock-config "$vsock")"
# The interface that exercises this feature: a NIC with an explicit MAC.
api PUT /network-interfaces/eth0 "$(python3 "$net_tool" network-config)"
if [[ "${HEPHAESTUS_TEST_MMDS:-0}" == 1 ]]; then
  api PUT /mmds '{"latest":{"meta-data":{"instance-id":"i-hephaestus-vmnet"}}}'
fi
api PUT /boot-source "$(python3 "$net_tool" boot-config \
  "$kernel" "$repo_root/build/agent.cpio.gz")"
api PUT /drives/rootfs "$(python3 "$net_tool" drive-config "$rootfs")"
api PUT /actions '{"action_type":"InstanceStart"}'

for _ in $(seq 1 100); do [[ -S "$vsock" ]] && break; sleep 0.1; done
if [[ ! -S "$vsock" ]]; then
  echo "vsock bridge did not create $vsock" >&2
  cat "$tmp/server.err" >&2 || true
  exit 1
fi

guest_check_args=()
if [[ "${HEPHAESTUS_TEST_MMDS:-0}" == 1 ]]; then
  guest_check_args+=(--mmds)
fi
python3 "$net_tool" check-guest "$vsock" "${guest_check_args[@]}"

echo "network attachment verified: $network_label"
echo "server log:  $tmp/server.err"
