#!/usr/bin/env bash
# Real-VM, headless e2e for guest networking on the Firecracker HTTP API
# path. Configures PUT /network-interfaces, boots, then asks the guest agent
# (over the /vsock CONNECT 1234 bridge) whether a non-loopback network device
# is present — i.e. whether the VZ NAT NIC actually reached the guest.
#
# We check for the device via sysfs rather than `ip`/DHCP so the smoke does
# not depend on the rootfs shipping iproute2 or a DHCP client: attaching the
# NIC is the VMM's job; configuring L3 is the guest image's.
#
# Requires real apple/container kernel/rootfs artifacts. Not wired into
# GitHub CI; run `just fc-compat-net-e2e` on a Mac that can run VZ VMs.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

cargo build -p hephaestus-firecracker
scripts/build-agent.sh

cdir="$HOME/Library/Application Support/com.apple.container"
kernel="$(ls "$cdir"/kernels/vmlinux-* 2>/dev/null | head -1 || true)"
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
log="$tmp/fc.log"
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

./build/cargo_target/debug/hephaestus-firecracker \
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
  curl -fsS --unix-socket "$sock" -X "$1" \
    -H 'content-type: application/json' \
    ${3:+--data "$3"} \
    "http://localhost$2" >/dev/null
}

api PUT /machine-config '{"vcpu_count":2,"mem_size_mib":512}'
api PUT /vsock "$(python3 -c "import json,sys; print(json.dumps({'guest_cid':3,'uds_path':sys.argv[1]}))" "$vsock")"
# The interface that exercises this feature: a NIC with an explicit MAC.
api PUT /network-interfaces/eth0 "$(python3 -c "print('{\"iface_id\":\"eth0\",\"host_dev_name\":\"tap0\",\"guest_mac\":\"AA:FC:00:00:00:01\"}')")"
api PUT /boot-source "$(python3 -c "import json,sys; print(json.dumps({'kernel_image_path':sys.argv[1],'initrd_path':sys.argv[2],'boot_args':'console=hvc0 rdinit=/init quiet loglevel=3'}))" "$kernel" "$repo_root/build/agent.cpio.gz")"
api PUT /drives/rootfs "$(python3 -c "import json,sys; print(json.dumps({'drive_id':'rootfs','path_on_host':sys.argv[1],'is_root_device':True,'is_read_only':False}))" "$rootfs")"
api PUT /actions '{"action_type":"InstanceStart"}'

for _ in $(seq 1 100); do [[ -S "$vsock" ]] && break; sleep 0.1; done
if [[ ! -S "$vsock" ]]; then
  echo "vsock bridge did not create $vsock" >&2
  cat "$tmp/server.err" >&2 || true
  exit 1
fi

python3 - "$vsock" <<'PY'
import socket, struct, sys, time
path = sys.argv[1]
# Exit 0 iff the guest has a network interface other than loopback — i.e. the
# VZ NAT NIC reached the guest. Pure sysfs; no iproute2/DHCP dependency.
cmd = b"test -n \"$(ls /sys/class/net 2>/dev/null | grep -v '^lo$')\""

def connect_with_retry(port):
    last = None
    for _ in range(160):
        try:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.connect(path)
            s.sendall(f"CONNECT {port}\n".encode())
            s.settimeout(0.05)
            try:
                data = s.recv(4, socket.MSG_PEEK)
                if data.startswith(b"ERR "):
                    raise RuntimeError(s.recv(256))
            except TimeoutError:
                pass
            finally:
                s.settimeout(None)
            return s
        except Exception as exc:
            last = exc
            time.sleep(0.25)
    raise RuntimeError(f"could not connect to guest port {port}: {last}")

last = None
for _ in range(80):
    try:
        command = connect_with_retry(1234)
        command.settimeout(30)
        command.sendall(struct.pack("<I", len(cmd)) + cmd)
        data = b""
        while len(data) < 4:
            chunk = command.recv(4 - len(data))
            if not chunk:
                raise RuntimeError("short exit-code read")
            data += chunk
        if data.startswith(b"ERR "):
            raise RuntimeError(data + command.recv(256))
        code = struct.unpack("<i", data)[0]
        if code != 0:
            raise RuntimeError(f"no non-loopback netdev in guest (agent exit {code}); NIC not attached")
        print("guest sees a non-loopback network device (VZ NAT NIC attached)")
        raise SystemExit(0)
    except Exception as exc:
        sys.stderr.write(f"net-e2e attempt failed: {type(exc).__name__}: {exc!r}\n")
        last = exc
        time.sleep(0.25)
raise SystemExit(f"could not complete net e2e: {last}")
PY

echo "server log:  $tmp/server.err"
