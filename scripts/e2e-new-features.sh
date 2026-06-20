#!/usr/bin/env bash
# Real-VM e2e for recently added local-only features:
#   1. `hephaestus vz-exec --stdin` host-stdin forwarding.
#   2. direct-VZ stdout/stderr split over hvc0/hvc1.
#   3. `hephaestus-jailer` launching a real `hephaestus-firecracker` daemon.
#
# Requires apple/container kernel/rootfs artifacts. This is intentionally not
# wired into GitHub-hosted CI; use `just e2e-new-features` on a Mac that can run
# VZ VMs.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

cargo build -p hephaestus-cli -p hephaestus-firecracker -p hephaestus-jailer
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

tmp="$(mktemp -d /tmp/heph-new-features.XXXXXX)"
jailer_pid=""
cleanup() {
  if [[ -n "$jailer_pid" ]]; then
    kill "$jailer_pid" 2>/dev/null || true
    wait "$jailer_pid" 2>/dev/null || true
  fi
  rm -rf "$tmp"
}
trap cleanup EXIT

clone_rootfs() {
  local name="$1"
  local dst="$tmp/$name.ext4"
  cp -c "$rootfs_src" "$dst"
  printf '%s\n' "$dst"
}

heph="$repo_root/build/cargo_target/debug/hephaestus"
firecracker="$repo_root/build/cargo_target/debug/hephaestus-firecracker"
jailer="$repo_root/build/cargo_target/debug/hephaestus-jailer"
initramfs="$repo_root/build/agent.cpio.gz"

# 1. vz-exec --stdin round-trip.
stdin_rootfs=$(clone_rootfs stdin)
stdin_token="heph-stdin-token-$(date +%s%N)"
stdin_out="$tmp/stdin.out"
stdin_err="$tmp/stdin.err"
printf '%s\n' "$stdin_token" | "$heph" vz-exec \
  --kernel "$kernel" \
  --rootfs "$stdin_rootfs" \
  --initramfs "$initramfs" \
  --cmd 'cat' \
  --stdin \
  >"$stdin_out" \
  2>"$stdin_err"
if ! grep -Fq "$stdin_token" "$stdin_out"; then
  echo "vz-exec --stdin did not echo token on stdout" >&2
  echo "--- stdout ---" >&2; cat "$stdin_out" >&2 || true
  echo "--- stderr ---" >&2; cat "$stdin_err" >&2 || true
  exit 1
fi
echo "vz-exec --stdin e2e passed"

# 2. stderr split. stdout and stderr are noisy with agent/kernel diagnostics, so
# use unique payload markers and assert they land on the expected host stream.
stderr_rootfs=$(clone_rootfs stderr)
stdout_token="heph-stdout-token-$(date +%s%N)"
stderr_token="heph-stderr-token-$(date +%s%N)"
split_out="$tmp/split.out"
split_err="$tmp/split.err"
"$heph" vz-exec \
  --kernel "$kernel" \
  --rootfs "$stderr_rootfs" \
  --initramfs "$initramfs" \
  --cmd "printf '%s\\n' '$stdout_token'; printf '%s\\n' '$stderr_token' >&2" \
  >"$split_out" \
  2>"$split_err"
if ! grep -Fq "$stdout_token" "$split_out"; then
  echo "stdout marker missing from host stdout" >&2
  echo "--- stdout ---" >&2; cat "$split_out" >&2 || true
  echo "--- stderr ---" >&2; cat "$split_err" >&2 || true
  exit 1
fi
if ! grep -Fq "$stderr_token" "$split_err"; then
  echo "stderr marker missing from host stderr" >&2
  echo "--- stdout ---" >&2; cat "$split_out" >&2 || true
  echo "--- stderr ---" >&2; cat "$split_err" >&2 || true
  exit 1
fi
if grep -Fq "$stderr_token" "$split_out"; then
  echo "stderr marker leaked onto host stdout" >&2
  echo "--- stdout ---" >&2; cat "$split_out" >&2 || true
  echo "--- stderr ---" >&2; cat "$split_err" >&2 || true
  exit 1
fi
echo "stderr-split e2e passed"

# 3. Jailer launches a real firecracker daemon, generates a profile under its
# per-VM work dir, and the Go SDK harness can cold-boot/pause against the socket.
jail_rootfs=$(clone_rootfs jailer)
jail_work="$tmp/jail-work"
jail_id="jail-e2e"
jail_dir="$jail_work/$jail_id"
jail_sock="$jail_dir/api.sock"
jail_log="$jail_dir/fc-compat.log"
"$jailer" \
  --id "$jail_id" \
  --work-dir "$jail_work" \
  --firecracker-binary "$firecracker" \
  --kernel "$kernel" \
  --rootfs "$jail_rootfs" \
  --initramfs "$initramfs" \
  >"$tmp/jailer.out" \
  2>"$tmp/jailer.err" &
jailer_pid=$!

for _ in $(seq 1 80); do
  [[ -S "$jail_sock" ]] && break
  if ! kill -0 "$jailer_pid" 2>/dev/null; then
    echo "hephaestus-jailer exited before creating $jail_sock" >&2
    echo "--- jailer stderr ---" >&2; cat "$tmp/jailer.err" >&2 || true
    exit 1
  fi
  sleep 0.1
done
if [[ ! -S "$jail_sock" ]]; then
  echo "hephaestus-jailer did not create $jail_sock" >&2
  echo "--- jailer stderr ---" >&2; cat "$tmp/jailer.err" >&2 || true
  exit 1
fi
if [[ ! -s "$jail_dir/sandbox.profile" ]]; then
  echo "jailer did not write $jail_dir/sandbox.profile" >&2
  echo "--- jailer stderr ---" >&2; cat "$tmp/jailer.err" >&2 || true
  exit 1
fi

if ! compat/firectl-harness/firectl-harness \
  -sock "$jail_sock" \
  -kernel "$kernel" \
  -rootfs "$jail_rootfs" \
  -log "$jail_log" \
  -pause \
  -skip-vsock; then
  echo "--- jailer stderr ---" >&2; cat "$tmp/jailer.err" >&2 || true
  echo "--- sandbox profile ---" >&2; cat "$jail_dir/sandbox.profile" >&2 || true
  exit 1
fi
echo "hephaestus-jailer real-VM e2e passed"

echo "new-feature real-VM e2e passed"
