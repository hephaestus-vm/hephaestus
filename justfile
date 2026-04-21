# hephaestus — macOS / Apple Silicon fork of Firecracker.
#
# Run `just` to list recipes. All recipes assume macOS 26 + Xcode 26 +
# apple/container installed (`brew install container`).
#
# The VM recipes expect `container system start` has been run once and the
# recommended kernel has been fetched; they then discover the cached
# kernel/initfs/rootfs from ~/Library/Application Support/com.apple.container/.

set shell := ["bash", "-uceo", "pipefail"]

bin := "./build/cargo_target/debug/hephaestus"
cdir := env_var('HOME') + "/Library/Application Support/com.apple.container"

# ───────── Default ─────────

# List available recipes.
default:
    @just --list --unsorted

# ───────── Build ─────────

# Compile the workspace. Auto-codesigns the binary via scripts/link-and-sign.sh.
build:
    cargo build -p hephaestus-cli

# `cargo clean` plus wipe the Swift build cache.
clean:
    cargo clean
    rm -rf swift/HephaestusBridge/.build

# ───────── Smoke tests (no VM) ─────────

# Ping/pong roundtrip through the Rust ⇄ Swift FFI.
ping: build
    {{bin}} ping

# Confirm the binary is signed with com.apple.security.virtualization.
verify-signing: build
    @codesign -d --entitlements - {{bin}} 2>&1 | grep -q virtualization \
        && echo "OK: signed with virtualization entitlement" \
        || { echo "FAIL: entitlement missing"; exit 1; }

# ───────── Artifact discovery ─────────

# Print the kernel/initfs/rootfs paths found in apple/container's cache.
artifacts:
    #!/usr/bin/env bash
    set -euo pipefail
    KERNEL="$(ls "{{cdir}}"/kernels/vmlinux-* 2>/dev/null | head -1 || true)"
    SNAPS=("{{cdir}}"/snapshots/*/snapshot)
    if [[ -z "$KERNEL" ]] || [[ ! -e "${SNAPS[0]:-}" ]]; then
        echo "no artifacts found under {{cdir}}" >&2
        echo "first run: container system start && container run --rm docker.io/library/alpine:3.20 echo hi" >&2
        exit 1
    fi
    INITFS=$(stat -f '%z %N' "${SNAPS[@]}" | sort -n  | head -1 | cut -d' ' -f2-)
    ROOTFS=$(stat -f '%z %N' "${SNAPS[@]}" | sort -nr | head -1 | cut -d' ' -f2-)
    printf 'kernel: %s\ninitfs: %s\nrootfs: %s\n' "$KERNEL" "$INITFS" "$ROOTFS"

# ───────── Run a VM ─────────

# No `--` prefix needed; argv is passed straight through. Shell quoting
# (spaces, `;`, `&&`) is dropped by just's variadic args — for shell chains
# invoke `scripts/run-vm.sh` directly, e.g.
#   scripts/run-vm.sh /bin/sh -c 'uname -a; ls /'
#
# Boot a VM and run a single command. Example: `just run /bin/cat /etc/hostname`.
run *args: build
    scripts/run-vm.sh {{args}}

# Preset: boot + echo, the canonical V1 smoke test.
hello: build
    scripts/run-vm.sh /bin/echo hello-from-hephaestus

# Preset: boot + a diagnostic shell chain inside the guest.
shell: build
    scripts/run-vm.sh /bin/sh -c 'uname -a; cat /etc/os-release; ls /'

# Preset: boot with networking on + try an outbound wget against example.com.
network-check: build
    HEPHAESTUS_NETWORK=1 scripts/run-vm.sh /bin/sh -c 'ip addr; ip route; wget -q -O- http://example.com | head -c 200'

# Drop into an interactive /bin/sh inside the guest with networking on.
# Use Ctrl-D or `exit` to leave.
sh: build
    HEPHAESTUS_NETWORK=1 HEPHAESTUS_TTY=1 scripts/run-vm.sh /bin/sh

# Interactive shell via the direct-VZ path (bypasses containerization / vminitd).
# No networking. `exit` or Ctrl-D to leave; the guest kernel halts on init exit.
vz-sh: build
    #!/usr/bin/env bash
    set -euo pipefail
    cdir="$HOME/Library/Application Support/com.apple.container"
    kernel="$(ls "$cdir"/kernels/vmlinux-* 2>/dev/null | head -1 || true)"
    snaps=("$cdir"/snapshots/*/snapshot)
    if [[ -z "$kernel" ]] || [[ ! -e "${snaps[0]:-}" ]]; then
        echo "no artifacts; run: just artifacts" >&2; exit 1
    fi
    rootfs=$(stat -f '%z %N' "${snaps[@]}" | sort -nr | head -1 | cut -d' ' -f2-)
    exec {{bin}} vz-sh --kernel "$kernel" --rootfs "$rootfs"

# Tail the kernel boot log from the last VM run (default id=dev).
bootlog id='dev':
    #!/usr/bin/env bash
    path=$(find "${TMPDIR:-/tmp}" -name "hephaestus-{{id}}.bootlog" 2>/dev/null | head -1)
    if [[ -z "$path" ]]; then
        echo "no bootlog for id={{id}}" >&2; exit 1
    fi
    echo "=== $path ==="
    tail -40 "$path"

# ───────── Rootfs helpers ─────────

# Convert a tar archive to an ext4 block device.
rootfs-build tar out size='512': build
    {{bin}} rootfs --from-tar {{tar}} --output {{out}} --size-mib {{size}}

# Run cargo unit tests + ping + test-rootfs. No VM boot; safe without artifacts.
test: build
    cargo test --workspace
    @just ping
    @just test-rootfs

# Sanity check: build a tiny tar, convert to ext4, run `file` on it.
test-rootfs: build
    #!/usr/bin/env bash
    set -euo pipefail
    src=/tmp/hephaestus-rfs-src
    rm -rf "$src" /tmp/hephaestus-rfs.ext4 /tmp/hephaestus-rfs.tgz
    mkdir -p "$src"/bin "$src"/etc
    echo 'hello from hephaestus' > "$src"/etc/motd
    tar -czf /tmp/hephaestus-rfs.tgz -C "$src" .
    {{bin}} rootfs --from-tar /tmp/hephaestus-rfs.tgz --output /tmp/hephaestus-rfs.ext4 --size-mib 64
    file /tmp/hephaestus-rfs.ext4
