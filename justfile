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

# Compile the workspace. Auto-codesigns binaries via scripts/link-and-sign.sh.
build:
    cargo build --workspace

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

# Assert cross-VM isolation on VZ NAT: two VMs, distinct leases in one /24,
# and neither TCP nor ICMP crosses between them (with a gateway-reachability
# control). Root-free, entitlement-free; needs apple/container artifacts.
# Measured `isolated` on macOS 26.5.2 — if this ever fails, an OS update
# changed VZ NAT's semantics and the threat model needs a revisit.
net-isolation-check: build build-agent
    HEPHAESTUS_EXPECT_CROSS_VM=isolated scripts/net-isolation-e2e.sh

# Same measurement across two per-VM vmnet networks (profile-authorized
# bundle required). Distinct /24 subnets and no cross-VM reachability are
# the product guarantee for the vmnet path.
vmnet-isolation-check: sign-vmnet build-agent
    HEPHAESTUS_FIRECRACKER_BIN="$PWD/build/HephaestusFirecracker.app/Contents/MacOS/hephaestus-firecracker" \
        HEPHAESTUS_FIRECRACKER_ARGS="--network-backend vmnet" \
        HEPHAESTUS_EXPECT_CROSS_VM=isolated \
        HEPHAESTUS_EXPECT_DISTINCT_SUBNETS=1 \
        scripts/net-isolation-e2e.sh

# Preset: boot with networking on + try an outbound wget against example.com.
network-check: build
    HEPHAESTUS_NETWORK=1 scripts/run-vm.sh /bin/sh -c 'ip addr; ip route; wget -q -O- http://example.com | head -c 200'

# Smoke test: run two network-enabled VMs with distinct ids concurrently and
# confirm they land on different IPs in 192.168.64.0/24. Each fetches its
# externally-visible IP so you can see both return independently.
parallel-net-check: build
    #!/usr/bin/env bash
    set -euo pipefail
    run_one() {
        local id="$1"
        HEPHAESTUS_NETWORK=1 HEPHAESTUS_ID="$id" scripts/run-vm.sh /bin/sh -c \
            "echo [$id] eth0=\$(ip -4 addr show dev eth0 | awk '/inet / {print \$2}'); wget -q -O- http://example.com | head -c 80"
    }
    ( run_one alpha ) &
    ( run_one beta ) &
    wait
    echo "both VMs completed"

# Drop into an interactive /bin/sh inside the guest with networking on.
# Use Ctrl-D or `exit` to leave.
sh: build
    HEPHAESTUS_NETWORK=1 HEPHAESTUS_TTY=1 scripts/run-vm.sh /bin/sh

# Cross-compile the guest agent (aarch64-linux-musl via zig) and pack it as a
# cpio.gz initramfs at build/agent.cpio.gz. Needed once per agent source change.
build-agent:
    scripts/build-agent.sh

# Pool default dir. Everything below honours $HEPHAESTUS_POOL_DIR if set.
pool_dir := env_var_or_default('HEPHAESTUS_POOL_DIR', '/tmp/hephaestus-pool')

# Initialize a warm pool of pre-snapshotted VMs.
# Usage: just pool-init       # 4 slots (default)
#        just pool-init 8     # positional arg == slot count
pool-init size='4': build build-agent
    #!/usr/bin/env bash
    set -euo pipefail
    cdir="$HOME/Library/Application Support/com.apple.container"
    kernel="$(ls "$cdir"/kernels/vmlinux-* 2>/dev/null | head -1 || true)"
    snaps=("$cdir"/snapshots/*/snapshot)
    rootfs_src=$(stat -f '%z %N' "${snaps[@]}" | sort -nr | head -1 | cut -d' ' -f2-)
    {{bin}} pool destroy --dir {{pool_dir}} 2>/dev/null || true
    {{bin}} pool init --dir {{pool_dir}} \
        --kernel "$kernel" --rootfs "$rootfs_src" \
        --size {{size}}

# Claim a warm slot and run a command inside the restored VM. Exits 75 if
# every slot is busy — caller owns retry/queueing (Firecracker-esque).
# Usage: just pool-run 'uname -a; echo hi'
pool-run cmd: build
    {{bin}} pool run --dir {{pool_dir}} --cmd {{quote(cmd)}}

# Show slot ready/busy counts.
pool-stats: build
    {{bin}} pool stats --dir {{pool_dir}}

# Tear down the pool dir.
pool-destroy: build
    {{bin}} pool destroy --dir {{pool_dir}}

# Pre-warm a direct-VZ VM with our agent listening on vsock and snapshot it.
# The saved VM is "ready to accept a command"; pair with `just vz-warm-run`.
vz-warm-save save='build/hh-warm.save': build build-agent
    #!/usr/bin/env bash
    set -euo pipefail
    cdir="$HOME/Library/Application Support/com.apple.container"
    kernel="$(ls "$cdir"/kernels/vmlinux-* 2>/dev/null | head -1 || true)"
    snaps=("$cdir"/snapshots/*/snapshot)
    rootfs_src=$(stat -f '%z %N' "${snaps[@]}" | sort -nr | head -1 | cut -d' ' -f2-)
    # Clone rootfs so the save doesn't mutate apple/container's state.
    rootfs="${TMPDIR:-/tmp}/hephaestus/vz-warm-rootfs.ext4"
    mkdir -p "$(dirname "$rootfs")"
    cp -c "$rootfs_src" "$rootfs"
    rm -f {{save}} {{save}}.machineid
    {{bin}} vz-warm save \
        --kernel "$kernel" --rootfs "$rootfs" \
        --save {{save}} --log "${TMPDIR:-/tmp}/hephaestus/vz-warm-save.log"

# Restore a pre-warmed VM and run an arbitrary command against it. Must be
# paired with an earlier `just vz-warm-save`. The rootfs + kernel passed
# here must match the ones used at save time.
vz-warm-run cmd save='build/hh-warm.save': build build-agent
    #!/usr/bin/env bash
    set -euo pipefail
    cdir="$HOME/Library/Application Support/com.apple.container"
    kernel="$(ls "$cdir"/kernels/vmlinux-* 2>/dev/null | head -1 || true)"
    rootfs="${TMPDIR:-/tmp}/hephaestus/vz-warm-rootfs.ext4"
    if [[ ! -e "$rootfs" ]] || [[ ! -e {{save}} ]]; then
        echo "no warm snapshot found; run: just vz-warm-save" >&2
        exit 1
    fi
    {{bin}} vz-warm run \
        --kernel "$kernel" --rootfs "$rootfs" \
        --save {{save}} --cmd {{quote(cmd)}} \
        --log "${TMPDIR:-/tmp}/hephaestus/vz-warm-run.log"

# Run a single guest command via the direct-VZ path + our own init agent.
# No vminitd, no containerization. Uses build/agent.cpio.gz (run `just build-agent` first).
# Usage: just vz-exec 'uname -a; echo $HOSTNAME'
vz-exec cmd: build build-agent
    #!/usr/bin/env bash
    set -euo pipefail
    cdir="$HOME/Library/Application Support/com.apple.container"
    kernel="$(ls "$cdir"/kernels/vmlinux-* 2>/dev/null | head -1 || true)"
    snaps=("$cdir"/snapshots/*/snapshot)
    if [[ -z "$kernel" ]] || [[ ! -e "${snaps[0]:-}" ]]; then
        echo "no artifacts; run: just artifacts" >&2; exit 1
    fi
    rootfs_src=$(stat -f '%z %N' "${snaps[@]}" | sort -nr | head -1 | cut -d' ' -f2-)
    # vz-exec mounts rootfs rw; clone so we don't mutate the shared snapshot.
    rootfs_dir="${TMPDIR:-/tmp}/hephaestus/vz-exec-rootfs"
    mkdir -p "$rootfs_dir"
    rootfs="$rootfs_dir/$(date +%s%N).ext4"
    cp -c "$rootfs_src" "$rootfs"
    trap 'rm -f "$rootfs"' EXIT
    {{bin}} vz-exec --kernel "$kernel" --rootfs "$rootfs" --cmd {{quote(cmd)}}

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

# ───────── Firecracker HTTP API compat ─────────

# Build the Go compat harness (drives hephaestus-firecracker via the same
# firecracker-go-sdk client that firectl/Kata use). Cached binary lives next
# to the source.
fc-harness-build:
    cd compat/firectl-harness && go build -o firectl-harness .

# CI-safe config-only compat smoke. Creates dummy kernel/rootfs files and
# passes -skip-boot, so it catches Firecracker API wire-shape drift without
# requiring apple/container artifacts or constructing a VZ VM.
fc-compat-config:
    scripts/fc-compat-config-only.sh

# CI-safe restrictive-sandbox compat smoke. Generates a deny-by-default macOS
# profile, proves an unrelated file is denied, then runs fc-compat-config's
# Go SDK harness with dummy artifacts and -skip-boot.
fc-compat-sandbox-config:
    scripts/fc-compat-sandbox-config.sh

# Real-VM compat smoke under a generated deny-by-default macOS sandbox profile.
# Requires apple/container kernel/rootfs artifacts; not CI-safe.
fc-compat-sandbox:
    scripts/fc-compat-sandbox.sh

# End-to-end compat smoke: starts hephaestus-firecracker on a fresh socket,
# replays the firectl request sequence (logger, machine-config, boot-source,
# drives, InstanceStart, PATCH /vm pause+resume), tears the server down.
# Pass `boot=0` to run the CI-safe config-only path with dummy artifacts.
fc-compat boot='1': build fc-harness-build
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ "{{boot}}" == "0" ]]; then
        exec scripts/fc-compat-config-only.sh
    fi
    cdir="$HOME/Library/Application Support/com.apple.container"
    kernel="$(ls "$cdir"/kernels/vmlinux-* 2>/dev/null | head -1 || true)"
    snaps=("$cdir"/snapshots/*/snapshot)
    if [[ -z "$kernel" ]] || [[ ! -e "${snaps[0]:-}" ]]; then
        echo "no artifacts; run: just artifacts" >&2; exit 1
    fi
    rootfs_src=$(stat -f '%z %N' "${snaps[@]}" | sort -nr | head -1 | cut -d' ' -f2-)
    rootfs="${TMPDIR:-/tmp}/hephaestus/fc-compat-rootfs.ext4"
    mkdir -p "$(dirname "$rootfs")"
    cp -c "$rootfs_src" "$rootfs"
    sock="${TMPDIR:-/tmp}/hephaestus-fc-compat.socket"
    log="${TMPDIR:-/tmp}/hephaestus/fc-compat.log"
    rm -f "$sock" "$log"
    {{bin}}-firecracker --api-sock "$sock" --id fc-compat &
    server=$!
    trap 'kill $server 2>/dev/null || true' EXIT
    # Wait for the listener to come up.
    for _ in $(seq 1 20); do [[ -S "$sock" ]] && break; sleep 0.1; done
    compat/firectl-harness/firectl-harness \
        -sock "$sock" -kernel "$kernel" -rootfs "$rootfs" \
        -log "$log" -pause

# Real-VM headless e2e for PUT /vsock's UDS bridge and guest-visible MMDS.
# Requires apple/container kernel/rootfs artifacts; not CI-safe.
fc-compat-vsock-e2e:
    scripts/fc-compat-vsock-e2e.sh

# Real-VM headless e2e for guest networking (PUT /network-interfaces -> VZ NAT
# NIC). Requires apple/container kernel/rootfs artifacts; not CI-safe.
fc-compat-net-e2e:
    scripts/fc-compat-net-e2e.sh

# Real-VM transparent MMDS smoke through the profile-authorized shared vmnet
# attachment. The guest obtains DHCP and fetches 169.254.169.254 without the
# agent MMDS shim. Requires the managed capability, profile, and identity.
fc-compat-vmnet-e2e: sign-vmnet
    HEPHAESTUS_FIRECRACKER_BIN="$PWD/build/HephaestusFirecracker.app/Contents/MacOS/hephaestus-firecracker" \
      HEPHAESTUS_FIRECRACKER_ARGS="--network-backend vmnet --host-mmds" \
      HEPHAESTUS_NETWORK_LABEL="shared vmnet + transparent MMDS" \
      HEPHAESTUS_TEST_MMDS=1 \
      scripts/fc-compat-net-e2e.sh

# Real-VM e2e for vz-exec --stdin, stderr split, and hephaestus-jailer.
# Requires apple/container kernel/rootfs artifacts; not CI-safe.
e2e-new-features:
    scripts/e2e-new-features.sh

# Same vsock/MMDS e2e under a generated deny-by-default sandbox profile.
fc-compat-sandbox-vsock-e2e:
    HEPHAESTUS_SANDBOX=1 scripts/fc-compat-vsock-e2e.sh

# Snapshot save/load round-trip under generated deny-by-default sandbox profiles.
fc-compat-sandbox-snapshot:
    scripts/fc-compat-sandbox-snapshot.sh

# Pool restore under generated deny-by-default sandbox profiles.
fc-compat-sandbox-pool:
    scripts/fc-compat-sandbox-pool.sh agent

# Stock-init pool restore under generated deny-by-default sandbox profiles.
fc-compat-sandbox-pool-stock:
    scripts/fc-compat-sandbox-pool.sh stock

# Pool-backed compat smoke. Initializes a 1-slot warm pool with the same
# kernel + rootfs + 2 CPU / 512 MiB tuple the harness asks for, then
# starts hephaestus-firecracker --pool-dir and runs the harness; the
# server log should show "pool hit slot=0 restore=...ms" instead of a
# cold boot.
fc-compat-pool: build build-agent fc-harness-build
    #!/usr/bin/env bash
    set -euo pipefail
    cdir="$HOME/Library/Application Support/com.apple.container"
    kernel="$(ls "$cdir"/kernels/vmlinux-* 2>/dev/null | head -1 || true)"
    snaps=("$cdir"/snapshots/*/snapshot)
    if [[ -z "$kernel" ]] || [[ ! -e "${snaps[0]:-}" ]]; then
        echo "no artifacts; run: just artifacts" >&2; exit 1
    fi
    rootfs_src=$(stat -f '%z %N' "${snaps[@]}" | sort -nr | head -1 | cut -d' ' -f2-)
    pool="${TMPDIR:-/tmp}/hephaestus-fc-pool"
    {{bin}} pool destroy --dir "$pool" 2>/dev/null || true
    {{bin}} pool init --dir "$pool" --kernel "$kernel" --rootfs "$rootfs_src" --size 1
    sock="${TMPDIR:-/tmp}/hephaestus-fc-pool.socket"
    log="${TMPDIR:-/tmp}/hephaestus/fc-pool.log"
    rm -f "$sock" "$log"
    {{bin}}-firecracker --api-sock "$sock" --id fc-pool --pool-dir "$pool" &
    server=$!
    trap 'kill $server 2>/dev/null || true; {{bin}} pool destroy --dir "$pool" 2>/dev/null || true' EXIT
    for _ in $(seq 1 20); do [[ -S "$sock" ]] && break; sleep 0.1; done
    # Match key needs the pool's pristine.ext4 path verbatim — pool's save
    # references that exact file, so the client must point at it (not the
    # apple/container snapshot the pool was *built from*).
    # Pool was warmed at the Swift defaults (2 CPU, 512 MiB) — match those
    # so the backend's strict tuple check hits.
    compat/firectl-harness/firectl-harness \
        -sock "$sock" -kernel "$kernel" -rootfs "$pool/pristine.ext4" \
        -log "$log" -pause \
        -vcpu 2 -mem 512 -mem-patch 512

# End-to-end snapshot round-trip: server A boots a VM cold, pauses it,
# saves via PUT /snapshot/create. Server B (fresh process) loads the
# saved blob via PUT /snapshot/load with resume_vm=true and verifies
# the restored VM is Running.
fc-compat-snapshot: build fc-harness-build
    #!/usr/bin/env bash
    set -euo pipefail
    cdir="$HOME/Library/Application Support/com.apple.container"
    kernel="$(ls "$cdir"/kernels/vmlinux-* 2>/dev/null | head -1 || true)"
    snaps=("$cdir"/snapshots/*/snapshot)
    if [[ -z "$kernel" ]] || [[ ! -e "${snaps[0]:-}" ]]; then
        echo "no artifacts; run: just artifacts" >&2; exit 1
    fi
    rootfs_src=$(stat -f '%z %N' "${snaps[@]}" | sort -nr | head -1 | cut -d' ' -f2-)
    rootfs="${TMPDIR:-/tmp}/hephaestus/fc-snap-rootfs.ext4"
    mkdir -p "$(dirname "$rootfs")"
    cp -c "$rootfs_src" "$rootfs"
    snap="${TMPDIR:-/tmp}/hephaestus/fc-snap.bin"
    rm -f "$snap" "$snap.machineid" "$snap.mem"
    sock_a="${TMPDIR:-/tmp}/hephaestus-fc-snap-a.socket"
    sock_b="${TMPDIR:-/tmp}/hephaestus-fc-snap-b.socket"
    log_a="${TMPDIR:-/tmp}/hephaestus/fc-snap-a.log"
    log_b="${TMPDIR:-/tmp}/hephaestus/fc-snap-b.log"
    rm -f "$sock_a" "$sock_b" "$log_a" "$log_b"

    # Phase 1: cold boot, pause, snapshot.
    # Pre-declare both PIDs so the EXIT trap (set -u) doesn't abort on an
    # unbound $server_b if phase 1 fails before phase 2 assigns it — which
    # would otherwise leak the phase-1 server.
    server_a="" server_b=""
    {{bin}}-firecracker --api-sock "$sock_a" --id fc-snap-a &
    server_a=$!
    trap 'kill ${server_a:-} ${server_b:-} 2>/dev/null || true' EXIT
    for _ in $(seq 1 20); do [[ -S "$sock_a" ]] && break; sleep 0.1; done
    compat/firectl-harness/firectl-harness \
        -sock "$sock_a" -kernel "$kernel" -rootfs "$rootfs" \
        -log "$log_a" -snapshot-save "$snap" \
        -vcpu 2 -mem 512 -mem-patch 512
    kill $server_a 2>/dev/null || true
    wait $server_a 2>/dev/null || true

    # Phase 2: fresh server, load snapshot, verify Running.
    {{bin}}-firecracker --api-sock "$sock_b" --id fc-snap-b &
    server_b=$!
    for _ in $(seq 1 20); do [[ -S "$sock_b" ]] && break; sleep 0.1; done
    compat/firectl-harness/firectl-harness \
        -sock "$sock_b" -kernel "$kernel" -rootfs "$rootfs" \
        -log "$log_b" -snapshot-load "$snap" -pause \
        -vcpu 2 -mem 512 -mem-patch 512

# Stock-init pool variant of fc-compat-pool. Snapshots the rootfs's own
# /bin/sh as PID 1 (no hephaestus-agent, no vsock, no initramfs) so
# restored VMs are behaviorally indistinguishable from cold-boot for the
# HTTP API consumer. This is the session-3.5 follow-up that closes the
# agent-init divergence.
fc-compat-pool-stock: build fc-harness-build
    #!/usr/bin/env bash
    set -euo pipefail
    cdir="$HOME/Library/Application Support/com.apple.container"
    kernel="$(ls "$cdir"/kernels/vmlinux-* 2>/dev/null | head -1 || true)"
    snaps=("$cdir"/snapshots/*/snapshot)
    if [[ -z "$kernel" ]] || [[ ! -e "${snaps[0]:-}" ]]; then
        echo "no artifacts; run: just artifacts" >&2; exit 1
    fi
    rootfs_src=$(stat -f '%z %N' "${snaps[@]}" | sort -nr | head -1 | cut -d' ' -f2-)
    pool="${TMPDIR:-/tmp}/hephaestus-fc-pool-stock"
    {{bin}} pool destroy --dir "$pool" 2>/dev/null || true
    {{bin}} pool init --dir "$pool" --kernel "$kernel" --rootfs "$rootfs_src" \
        --size 1 --stock-init --settle-seconds 3
    sock="${TMPDIR:-/tmp}/hephaestus-fc-pool-stock.socket"
    log="${TMPDIR:-/tmp}/hephaestus/fc-pool-stock.log"
    rm -f "$sock" "$log"
    {{bin}}-firecracker --api-sock "$sock" --id fc-pool-stock --pool-dir "$pool" &
    server=$!
    trap 'kill $server 2>/dev/null || true; {{bin}} pool destroy --dir "$pool" 2>/dev/null || true' EXIT
    for _ in $(seq 1 20); do [[ -S "$sock" ]] && break; sleep 0.1; done
    compat/firectl-harness/firectl-harness \
        -sock "$sock" -kernel "$kernel" -rootfs "$pool/pristine.ext4" \
        -log "$log" -pause -skip-vsock \
        -vcpu 2 -mem 512 -mem-patch 512

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

# --- M1b (vmnet MMDS) enablement ------------------------------------------
# Probe whether this machine can honor the restricted com.apple.vm.networking
# entitlement. The executable is wrapped in an app bundle because AMFI requires
# the authorizing provisioning profile to be embedded. See
# docs/development/privileged-features.md.
probe-vmnet:
    #!/usr/bin/env bash
    set -euo pipefail
    tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
    sdk="$(env -u SDKROOT xcrun --sdk macosx --show-sdk-path)"
    env -u SDKROOT xcrun --sdk macosx swiftc -sdk "$sdk" -O \
      scripts/vmnet-probe.swift -o "$tmp/vmnet-probe"
    scripts/package-vmnet-app.sh "$tmp/vmnet-probe" "$tmp/VMNetProbe.app"
    echo "== running profile-authorized vmnet probe =="
    "$tmp/VMNetProbe.app/Contents/MacOS/vmnet-probe"

# Build hephaestus-firecracker and package it in an app bundle carrying the
# provisioning profile that authorizes com.apple.vm.networking. Run the daemon
# from build/HephaestusFirecracker.app/Contents/MacOS/hephaestus-firecracker.
sign-vmnet:
    #!/usr/bin/env bash
    set -euo pipefail
    env -u SDKROOT cargo build -p hephaestus-firecracker
    scripts/package-vmnet-app.sh \
      build/cargo_target/debug/hephaestus-firecracker \
      build/HephaestusFirecracker.app

# --- M4 (jailer productionization) ----------------------------------------
# Verify the jailer's --rlimit-* caps reach the exec'd daemon. Root-free,
# VM-free (uses a stand-in binary that prints its own ulimits).
jailer-rlimit-check: build
    #!/usr/bin/env bash
    set -euo pipefail
    j="./build/cargo_target/debug/hephaestus-jailer"
    tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
    touch "$tmp/vmlinux" "$tmp/rootfs.ext4"
    printf '#!/bin/sh\necho "NOFILE=$(ulimit -n)"\nexit 0\n' > "$tmp/fake-fc"
    chmod +x "$tmp/fake-fc"
    out="$("$j" --id rl-check --work-dir "$tmp/work" --kernel "$tmp/vmlinux" \
        --rootfs "$tmp/rootfs.ext4" --firecracker-binary "$tmp/fake-fc" \
        --rlimit-nofile 64 2>/dev/null)"
    [[ "$out" == "NOFILE=64" ]] \
        && echo "OK: jailer applied RLIMIT_NOFILE=64 to the daemon" \
        || { echo "FAIL: got '$out'"; exit 1; }

# Verify the jailer's per-VM lifecycle ownership: the instance lock refuses
# a second same-id launch, a stale api.sock is replaced, and --teardown
# retires the on-disk state. Root-free, VM-free (stand-in daemon binaries).
jailer-lifecycle-check: build
    #!/usr/bin/env bash
    set -euo pipefail
    j="./build/cargo_target/debug/hephaestus-jailer"
    tmp="$(mktemp -d)"; first=""
    trap '[[ -n "$first" ]] && kill "$first" 2>/dev/null; rm -rf "$tmp"' EXIT
    touch "$tmp/vmlinux" "$tmp/rootfs.ext4"
    printf '#!/bin/sh\nsleep 30\n' > "$tmp/sleep-fc"; chmod +x "$tmp/sleep-fc"
    printf '#!/bin/sh\nexit 0\n' > "$tmp/exit-fc"; chmod +x "$tmp/exit-fc"
    echo "--- A second jailer for the same --id must be refused ---"
    "$j" --id lc-check --work-dir "$tmp/work" --kernel "$tmp/vmlinux" \
        --rootfs "$tmp/rootfs.ext4" --firecracker-binary "$tmp/sleep-fc" \
        2>/dev/null & first=$!
    sleep 1
    if out="$("$j" --id lc-check --work-dir "$tmp/work" --kernel "$tmp/vmlinux" \
        --rootfs "$tmp/rootfs.ext4" --firecracker-binary "$tmp/exit-fc" 2>&1)"; then
        echo "FAIL: second same-id jailer was not refused"; exit 1
    fi
    echo "$out" | grep -q "already running" \
        && echo "OK: second claim refused while the instance runs" \
        || { echo "FAIL: unexpected refusal: $out"; exit 1; }
    kill "$first"; wait "$first" 2>/dev/null || true; first=""
    echo "--- A stale api.sock is removed on the next launch ---"
    touch "$tmp/work/lc-check/api.sock"
    "$j" --id lc-check --work-dir "$tmp/work" --kernel "$tmp/vmlinux" \
        --rootfs "$tmp/rootfs.ext4" --firecracker-binary "$tmp/exit-fc" \
        2>&1 | grep -q "removed stale api socket" \
        && echo "OK: stale socket removed" \
        || { echo "FAIL: stale socket not removed"; exit 1; }
    echo "--- --teardown retires the work dir, profile, and lock ---"
    "$j" --id lc-check --work-dir "$tmp/work" --teardown 2>/dev/null
    for leftover in "$tmp/work/lc-check" \
        "$tmp/work/.lc-check.sandbox.profile" "$tmp/work/.lc-check.lock"; do
        [[ -e "$leftover" ]] && { echo "FAIL: $leftover survived teardown"; exit 1; }
    done
    echo "OK: teardown removed the instance state"

# Verify per-VM dedicated uid allocation (--uid-base): distinct uids per id,
# allocation persistence, the live-shared-uid refusal + --allow-shared-uid,
# and registry release on --teardown. Requires sudo; VM-free.
jailer-uid-check: build
    #!/usr/bin/env bash
    set -euo pipefail
    j="./build/cargo_target/debug/hephaestus-jailer"
    base=61000
    for probe in $base $((base+1)); do
        if id "$probe" >/dev/null 2>&1; then
            echo "SKIP: uid $probe belongs to a real account; set a different base"
            exit 0
        fi
    done
    # World-traversable /tmp for the same setuid path-traversal reason as
    # jailer-privdrop-check.
    tmp="$(mktemp -d /tmp/hephaestus-uidcheck.XXXXXX)"
    chmod a+rx "$tmp"
    trap 'sudo rm -rf "$tmp"' EXIT
    touch "$tmp/vmlinux" "$tmp/rootfs.ext4"
    printf '#!/bin/sh\necho "UID=$(id -u) GID=$(id -g)"\nexit 0\n' > "$tmp/fake-fc"
    printf '#!/bin/sh\necho "UID=$(id -u) GID=$(id -g)"\nsleep 5\n' > "$tmp/sleep-fc"
    chmod a+rx "$tmp/fake-fc" "$tmp/sleep-fc"
    run() { sudo "$j" --work-dir "$tmp/work" --kernel "$tmp/vmlinux" \
        --rootfs "$tmp/rootfs.ext4" "$@"; }
    echo "--- Each id gets its own uid from --uid-base $base ---"
    out="$(run --id uid-a --firecracker-binary "$tmp/fake-fc" --uid-base $base 2>&1)"
    echo "$out" | grep -q "UID=$base GID=$base" \
        && echo "OK: uid-a allocated $base" \
        || { echo "FAIL: uid-a got: $out"; exit 1; }
    out="$(run --id uid-b --firecracker-binary "$tmp/fake-fc" --uid-base $base 2>&1)"
    echo "$out" | grep -q "UID=$((base+1)) GID=$((base+1))" \
        && echo "OK: uid-b allocated $((base+1)) (uid-a's entry persists)" \
        || { echo "FAIL: uid-b got: $out"; exit 1; }
    echo "--- A live instance's uid is refused to others ---"
    run --id uid-a --firecracker-binary "$tmp/sleep-fc" --uid-base $base \
        2>/dev/null & first=$!
    sleep 1
    if out="$(run --id uid-c --firecracker-binary "$tmp/fake-fc" --uid $base 2>&1)"; then
        echo "FAIL: same-uid launch beside live uid-a was not refused"; exit 1
    fi
    echo "$out" | grep -q "same uid" \
        && echo "OK: live shared uid refused" \
        || { echo "FAIL: unexpected refusal: $out"; exit 1; }
    out="$(run --id uid-c --firecracker-binary "$tmp/fake-fc" --uid $base \
        --allow-shared-uid 2>&1)"
    echo "$out" | grep -q "UID=$base" \
        && echo "OK: --allow-shared-uid overrides" \
        || { echo "FAIL: override run got: $out"; exit 1; }
    # Backgrounding the `run` function backgrounds a subshell, and killing a
    # subshell does not signal its sudo/jailer/daemon children — so don't
    # kill at all: the stand-in daemon exits on its own, and `wait` is the
    # deterministic release of uid-a's flock.
    wait "$first" 2>/dev/null || true
    echo "--- Teardown releases the registry entries ---"
    for id in uid-a uid-b uid-c; do
        out="$(run --id "$id" --teardown 2>&1)" \
            || { echo "FAIL: teardown $id: $out"; exit 1; }
    done
    if sudo grep -qE "^uid-(a|b|c) " "$tmp/work/.uid-allocations"; then
        echo "FAIL: registry still holds torn-down entries"; exit 1
    fi
    echo "OK: registry entries released"

# Verify the jailer's --uid/--gid/--user privilege drop. Requires sudo.
# Uses a stand-in binary that prints its own uid/gid.
jailer-privdrop-check: build
    #!/usr/bin/env bash
    set -euo pipefail
    j="./build/cargo_target/debug/hephaestus-jailer"
    # Must live under world-traversable /tmp, NOT `mktemp -d`'s default
    # $TMPDIR: darwin per-user temp dirs (/var/folders/.../T) are 0700, so
    # after the jailer setuids to the target user the child's exec would
    # fail path traversal with EACCES before reaching the fake binary.
    tmp="$(mktemp -d /tmp/hephaestus-privdrop.XXXXXX)"
    chmod a+rx "$tmp"
    trap 'sudo rm -rf "$tmp"' EXIT
    touch "$tmp/vmlinux" "$tmp/rootfs.ext4"
    printf '#!/bin/sh\necho "UID=$(id -u) GID=$(id -g)"\nexit 0\n' > "$tmp/fake-fc"
    chmod a+rx "$tmp/fake-fc"
    user="${HEPHAESTUS_TEST_USER:-nobody}"
    expected_uid="$(id -u "$user")"
    expected_gid="$(id -g "$user")"
    echo "--- Testing --user $user (expect uid=$expected_uid gid=$expected_gid) ---"
    out="$(sudo "$j" --id priv-check --work-dir "$tmp/work" --kernel "$tmp/vmlinux" \
        --rootfs "$tmp/rootfs.ext4" --firecracker-binary "$tmp/fake-fc" \
        --user "$user" 2>&1)" || true
    echo "$out"
    # The jailer prints the child's stdout before its own stderr. Extract the
    # last line that looks like "UID=... GID=..." (the child's output).
    child_out="$(echo "$out" | grep -E '^UID=[0-9]+ GID=[0-9]+$' | tail -1 || true)"
    [[ "$child_out" == "UID=$expected_uid GID=$expected_gid" ]] \
        && echo "OK: jailer dropped to $user (uid=$expected_uid gid=$expected_gid)" \
        || { echo "FAIL: got '$child_out' (expected uid=$expected_uid gid=$expected_gid)"; exit 1; }
    echo "--- Testing --uid $expected_uid --gid $expected_gid ---"
    out="$(sudo "$j" --id priv-check-ug --work-dir "$tmp/work" --kernel "$tmp/vmlinux" \
        --rootfs "$tmp/rootfs.ext4" --firecracker-binary "$tmp/fake-fc" \
        --uid "$expected_uid" --gid "$expected_gid" 2>&1)" || true
    echo "$out"
    child_out="$(echo "$out" | grep -E '^UID=[0-9]+ GID=[0-9]+$' | tail -1 || true)"
    [[ "$child_out" == "UID=$expected_uid GID=$expected_gid" ]] \
        && echo "OK: jailer dropped to uid=$expected_uid gid=$expected_gid" \
        || { echo "FAIL: got '$child_out'"; exit 1; }
