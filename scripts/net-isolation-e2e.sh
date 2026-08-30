#!/usr/bin/env bash
# Real-VM cross-VM network isolation probe on the Firecracker HTTP API path.
#
# Boots TWO daemons (one process per VM, as always), gives each a NIC and
# DHCP, then asks VM B to ping VM A's address. B's exit code is the
# measurement. VM A reports its address through the agent's 4-byte exit-code
# channel — one octet per command — because the agent protocol carries no
# stdout, and serial-log scraping is timing-fragile.
#
# HEPHAESTUS_EXPECT_CROSS_VM steers the verdict:
#   report     print the measurement and exit 0 (probe mode)
#   reachable  fail unless B reached A
#   isolated   fail if B reached A
# HEPHAESTUS_EXPECT_DISTINCT_SUBNETS=1 additionally requires the two VMs'
# /24s to differ (the per-VM vmnet guarantee).
#
# Requires real apple/container kernel/rootfs artifacts. Not CI-safe; run
# via `just net-isolation-check` (NAT) or `just vmnet-isolation-check`.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
net_tool="$repo_root/scripts/fc_compat_net_e2e.py"
cd "$repo_root"

cargo build -p hephaestus-firecracker
scripts/build-agent.sh

firecracker="${HEPHAESTUS_FIRECRACKER_BIN:-./build/cargo_target/debug/hephaestus-firecracker}"
read -r -a firecracker_args <<< "${HEPHAESTUS_FIRECRACKER_ARGS:-}"
expect="${HEPHAESTUS_EXPECT_CROSS_VM:-report}"

cdir="$HOME/Library/Application Support/com.apple.container"
kernel="$(find "$cdir/kernels" -maxdepth 1 -type f -name 'vmlinux-*' -print -quit 2>/dev/null || true)"
snaps=("$cdir"/snapshots/*/snapshot)
if [[ -z "$kernel" ]] || [[ ! -e "${snaps[0]:-}" ]]; then
  echo "no artifacts; run: just artifacts" >&2
  exit 1
fi
rootfs_src=$(stat -f '%z %N' "${snaps[@]}" | sort -nr | head -1 | cut -d' ' -f2-)

tmp="$(mktemp -d /tmp/heph-net-iso.XXXXXX)"
servers=()
cleanup() {
  for pid in "${servers[@]}"; do
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  done
  if [[ "${HEPHAESTUS_KEEP_TMP:-0}" == 1 ]]; then
    echo "kept e2e directory: $tmp" >&2
  else
    rm -rf "$tmp"
  fi
}
trap cleanup EXIT

# Boot one VM: its own work dir, api socket, vsock bridge, rootfs clone,
# and a distinct guest MAC (two VMs on one segment must never share one).
boot_vm() {
  local id="$1" mac="$2"
  local dir="$tmp/$id" sock vsock rootfs
  mkdir -p "$dir"
  sock="$dir/fc.sock"; vsock="$dir/guest-vsock.sock"; rootfs="$dir/rootfs.ext4"
  cp -c "$rootfs_src" "$rootfs"
  HEPHAESTUS_FC_WORK_DIR="$dir" "$firecracker" "${firecracker_args[@]}" \
    --api-sock "$sock" --id "$id" \
    >"$dir/server.out" 2>"$dir/server.err" &
  servers+=($!)
  for _ in $(seq 1 50); do [[ -S "$sock" ]] && break; sleep 0.1; done
  [[ -S "$sock" ]] || { echo "$id: daemon did not create $sock" >&2; cat "$dir/server.err" >&2; exit 1; }

  api() {
    local body="$dir/api-response" status
    status="$(curl -sS -o "$body" -w '%{http_code}' --unix-socket "$sock" -X "$1" \
      -H 'content-type: application/json' ${3:+--data "$3"} "http://localhost$2")"
    [[ "$status" =~ ^2 ]] || { echo "$id: API $1 $2 -> HTTP $status: $(cat "$body")" >&2; return 1; }
  }
  api PUT /machine-config '{"vcpu_count":1,"mem_size_mib":256}'
  api PUT /vsock "$(python3 "$net_tool" vsock-config "$vsock")"
  api PUT /network-interfaces/eth0 "$(python3 "$net_tool" network-config --mac "$mac")"
  api PUT /boot-source "$(python3 "$net_tool" boot-config "$kernel" "$repo_root/build/agent.cpio.gz")"
  api PUT /drives/rootfs "$(python3 "$net_tool" drive-config "$rootfs")"
  api PUT /actions '{"action_type":"InstanceStart"}'
  for _ in $(seq 1 100); do [[ -S "$vsock" ]] && break; sleep 0.1; done
  [[ -S "$vsock" ]] || { echo "$id: vsock bridge missing" >&2; cat "$dir/server.err" >&2; exit 1; }
}

guest() { python3 "$net_tool" run-guest "$tmp/$1/guest-vsock.sock" "$2"; }
guest_no_wait() { python3 "$net_tool" run-guest --no-wait "$tmp/$1/guest-vsock.sock" "$2" >/dev/null; }

# The agent serves exactly ONE command per boot and powers the VM off after
# replying, so each VM gets a single composite command. VM A prints its
# address to the console (the serial log) and then sleeps so it is still
# alive when B probes it; A's exit code is never collected. VM B does its
# whole job — DHCP, print address, ping A — in its one exchange, and its
# exit code is the measurement.
# shellcheck disable=SC2016 # the $() must expand in the guest, not here
dhcp_cmd='iface="$(ls /sys/class/net | grep -v "^lo$" | head -1)"; ip link set "$iface" up; udhcpc -i "$iface" -n -q >/dev/null 2>&1; addr="$(ip -4 addr show dev "$iface" | awk "/inet /{print \$2}" | cut -d/ -f1 | head -1)"; echo "PROBE-IP=$addr"'

serial_log() { echo "$tmp/$1/hephaestus-firecracker-$1.log"; }

# The console line may lag the command; retry the grep.
read_probe_ip() {
  local id="$1" ip=""
  for _ in $(seq 1 120); do
    ip="$(grep -oE 'PROBE-IP=[0-9.]+' "$(serial_log "$id")" 2>/dev/null | head -1 | cut -d= -f2 || true)"
    [[ -n "$ip" ]] && { echo "$ip"; return 0; }
    sleep 0.5
  done
  echo "$id: no PROBE-IP line in $(serial_log "$id")" >&2
  return 1
}

boot_vm iso-a AA:FC:00:00:00:0A
boot_vm iso-b AA:FC:00:00:00:0B

# VM A: DHCP, report its address, run a TCP listener loop (busybox nc — the
# rootfs ships no httpd), and stay alive while B probes. The sleep bounds
# the whole run; A's liveness is re-verified after the measurement.
# shellcheck disable=SC2016 # guest-side expansions
listener_cmd='(while true; do echo ok | nc -l -p 7777 >/dev/null 2>&1; done) & sleep 300'
guest_no_wait iso-a "$dhcp_cmd; $listener_cmd"
ip_a="$(read_probe_ip iso-a)"
echo "iso-a address: $ip_a"

# VM B: control first (its own default gateway must answer, proving B's
# network works — otherwise "isolated" would be indistinguishable from a
# broken probe), then try A over TCP and ICMP separately so the log shows
# which legs actually ran.
# Exit meanings: 0 = TCP reached A; 3 = ICMP only; 1 = gateway fine, A
# unreachable on both; 2 = no gateway (invalid measurement).
# shellcheck disable=SC2016 # guest-side expansions
probe_cmd='gw="$(ip route 2>/dev/null | awk "/default/{print \$3}" | head -1)"; test -n "$gw" || exit 2; ping -c 2 -W 3 "$gw" >/dev/null 2>&1 || exit 2'
probe_code="$(guest iso-b "$dhcp_cmd; $probe_cmd; nc -w 3 $ip_a 7777 </dev/null >/dev/null 2>&1 && exit 0; ping -c 2 -W 3 $ip_a >/dev/null 2>&1 && exit 3; exit 1")"
ip_b="$(read_probe_ip iso-b)"
echo "iso-b address: $ip_b"
[[ "$ip_a" == "$ip_b" ]] && { echo "FAIL: both VMs report the same address" >&2; exit 1; }
if [[ "$probe_code" == 2 ]]; then
  echo "FAIL: iso-b cannot reach its own gateway — measurement invalid" >&2
  exit 1
fi

# A dead VM A is indistinguishable from an isolated one, so the verdict is
# only valid while A is still running. The daemon's instance-info endpoint
# answers on A's API socket.
state="$(curl -sS --unix-socket "$tmp/iso-a/fc.sock" http://localhost/ 2>/dev/null || true)"
if ! grep -q '"state":"Running"' <<<"$state"; then
  echo "FAIL: iso-a is not running after the probe (state: $state) — measurement invalid" >&2
  exit 1
fi

if [[ "${HEPHAESTUS_EXPECT_DISTINCT_SUBNETS:-0}" == 1 ]]; then
  if [[ "${ip_a%.*}" == "${ip_b%.*}" ]]; then
    echo "FAIL: expected distinct /24 subnets, both VMs are in ${ip_a%.*}.0/24" >&2
    exit 1
  fi
  echo "OK: VMs occupy distinct /24 subnets"
fi

case "$probe_code" in
  0) measured=reachable; legs="TCP";;
  3) measured=reachable; legs="ICMP only — TCP refused";;
  *) measured=isolated;  legs="TCP+ICMP both blocked";;
esac
echo "cross-VM measurement: iso-b -> iso-a ($ip_a) is $measured ($legs; gateway control passed)"

case "$expect" in
  report) echo "probe mode: no assertion (set HEPHAESTUS_EXPECT_CROSS_VM to lock this in)";;
  reachable|isolated)
    if [[ "$measured" == "$expect" ]]; then
      echo "OK: cross-VM reachability is '$measured' as expected"
    else
      echo "FAIL: measured '$measured', expected '$expect'" >&2
      exit 1
    fi;;
  *) echo "unknown HEPHAESTUS_EXPECT_CROSS_VM: $expect" >&2; exit 1;;
esac
