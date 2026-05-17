#!/usr/bin/env bash
# Real-VM, headless e2e for the Firecracker /vsock UDS bridge and
# guest-visible MMDS vsock service.
#
# Requires real apple/container kernel/rootfs artifacts. This is intentionally
# not wired into GitHub-hosted CI; use `just fc-compat-vsock-e2e` on a Mac that
# can run VZ VMs.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

cargo build -p hephaestus-firecracker
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

tmp="$(mktemp -d /tmp/heph-vsock-e2e.XXXXXX)"
sock="$tmp/fc.sock"
vsock="$tmp/guest-vsock.sock"
rootfs="$tmp/rootfs.ext4"
log="$tmp/fc.log"
serial="$tmp/serial.log"
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
  --id fc-vsock-e2e \
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

api PUT /logger "$(python3 - <<PY
import json
print(json.dumps({"log_path":"$log","level":"Debug","show_level":True,"show_log_origin":True}))
PY
)"
api PUT /metrics "$(python3 - <<PY
import json
print(json.dumps({"metrics_path":"$log.metrics"}))
PY
)"
api PUT /machine-config '{"vcpu_count":2,"mem_size_mib":512}'
api PUT /mmds/config '{"network_interfaces":[],"version":"V2","ipv4_address":"169.254.169.254"}'
api PUT /mmds '{"latest":{"meta-data":{"instance-id":"i-hephaestus-vsock-e2e"}}}'
api PUT /vsock "$(python3 - <<PY
import json
print(json.dumps({"guest_cid":3,"uds_path":"$vsock"}))
PY
)"
api PUT /boot-source "$(python3 - <<PY
import json
print(json.dumps({
  "kernel_image_path":"$kernel",
  "initrd_path":"$repo_root/build/agent.cpio.gz",
  "boot_args":"console=hvc0 rdinit=/init quiet loglevel=3"
}))
PY
)"
api PUT /drives/rootfs "$(python3 - <<PY
import json
print(json.dumps({"drive_id":"rootfs","path_on_host":"$rootfs","is_root_device":True,"is_read_only":False}))
PY
)"
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
cmd = b"__hephaestus_test_mmds_vsock i-hephaestus-vsock-e2e"
last = None
for _ in range(80):
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.connect(path)
        s.sendall(b"CONNECT 1234\n" + struct.pack("<I", len(cmd)) + cmd)
        data = b""
        while len(data) < 4:
            chunk = s.recv(4 - len(data))
            if not chunk:
                raise RuntimeError("short exit-code read")
            data += chunk
        if data.startswith(b"ERR "):
            raise RuntimeError(data + s.recv(256))
        code = struct.unpack("<i", data)[0]
        if code != 0:
            raise RuntimeError(f"guest MMDS vsock test exited {code}")
        print("guest MMDS vsock test exited 0")
        raise SystemExit(0)
    except Exception as exc:
        last = exc
        time.sleep(0.25)
raise SystemExit(f"could not complete vsock e2e: {last}")
PY

echo "serial log: $serial"
echo "server log:  $tmp/server.err"
