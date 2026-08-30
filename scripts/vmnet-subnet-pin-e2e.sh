#!/usr/bin/env bash
# Verify the per-VM vmnet subnet pin: two boots of the same instance (same
# work dir, same log path) must land on the SAME subnet, because the first
# boot records it in the `.vmnet-subnet` sidecar and the second pins from
# it. This is the property that keeps a restored guest's DHCP lease valid.
#
# Requires the profile-authorized app bundle. Run via
# `just vmnet-subnet-pin-check`.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
net_tool="$repo_root/scripts/fc_compat_net_e2e.py"
cd "$repo_root"

scripts/build-agent.sh

firecracker="${HEPHAESTUS_FIRECRACKER_BIN:?set HEPHAESTUS_FIRECRACKER_BIN to the app-bundle binary}"

cdir="$HOME/Library/Application Support/com.apple.container"
kernel="$(find "$cdir/kernels" -maxdepth 1 -type f -name 'vmlinux-*' -print -quit 2>/dev/null || true)"
snaps=("$cdir"/snapshots/*/snapshot)
if [[ -z "$kernel" ]] || [[ ! -e "${snaps[0]:-}" ]]; then
  echo "no artifacts; run: just artifacts" >&2
  exit 1
fi
rootfs_src=$(stat -f '%z %N' "${snaps[@]}" | sort -nr | head -1 | cut -d' ' -f2-)

tmp="$(mktemp -d /tmp/heph-subnet-pin.XXXXXX)"
server=""
cleanup() {
  if [[ -n "$server" ]]; then
    kill "$server" 2>/dev/null || true
    wait "$server" 2>/dev/null || true
  fi
  rm -rf "$tmp"
}
trap cleanup EXIT

cp -c "$rootfs_src" "$tmp/rootfs.ext4"

# One boot: start the daemon, configure, InstanceStart, harvest the subnet
# line from stderr, then tear the daemon down. The subnet line is emitted
# when the vmnet network is created during InstanceStart.
boot_once() {
  local round="$1" sock="$tmp/fc-$1.sock" err="$tmp/server-$1.err"
  HEPHAESTUS_FC_WORK_DIR="$tmp" "$firecracker" --network-backend vmnet \
    --api-sock "$sock" --id pin-check >"$tmp/server-$round.out" 2>"$err" &
  server=$!
  for _ in $(seq 1 50); do [[ -S "$sock" ]] && break; sleep 0.1; done
  [[ -S "$sock" ]] || { echo "round $round: no api socket" >&2; cat "$err" >&2; exit 1; }

  api() {
    local body="$tmp/api-response" status
    status="$(curl -sS -o "$body" -w '%{http_code}' --unix-socket "$sock" -X "$1" \
      -H 'content-type: application/json' ${3:+--data "$3"} "http://localhost$2")"
    [[ "$status" =~ ^2 ]] || { echo "round $round: API $1 $2 -> $status: $(cat "$body")" >&2; return 1; }
  }
  api PUT /machine-config '{"vcpu_count":1,"mem_size_mib":256}'
  api PUT /network-interfaces/eth0 "$(python3 "$net_tool" network-config)"
  api PUT /boot-source "$(python3 "$net_tool" boot-config "$kernel" "$repo_root/build/agent.cpio.gz")"
  api PUT /drives/rootfs "$(python3 "$net_tool" drive-config "$tmp/rootfs.ext4")"
  api PUT /actions '{"action_type":"InstanceStart"}'

  local line=""
  for _ in $(seq 1 100); do
    line="$(grep -o 'vmnet network subnet=[0-9.]* mask=[0-9.]*' "$err" | head -1 || true)"
    [[ -n "$line" ]] && break
    sleep 0.1
  done
  kill "$server" 2>/dev/null || true
  wait "$server" 2>/dev/null || true
  server=""
  [[ -n "$line" ]] || { echo "round $round: no subnet line in $err" >&2; cat "$err" >&2; exit 1; }
  echo "$line"
}

first="$(boot_once 1)"
echo "boot 1: $first"
[[ -f "$tmp/hephaestus-firecracker-pin-check.vmnet-subnet" ]] \
  || { echo "FAIL: subnet sidecar was not written" >&2; exit 1; }
second="$(boot_once 2)"
echo "boot 2: $second"

if [[ "$first" == "$second" ]]; then
  echo "OK: subnet pinned across boots via the sidecar"
else
  echo "FAIL: subnet changed across boots ('$first' vs '$second')" >&2
  exit 1
fi
