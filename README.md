# hephaestus

A macOS / Apple Silicon fork of [Firecracker](https://github.com/firecracker-microvm/firecracker),
re-targeted at Apple's [Virtualization.framework](https://developer.apple.com/documentation/virtualization)
via the [`apple/containerization`](https://github.com/apple/containerization)
Swift package.

> Status: experimental. V1 boots a Linux VM and runs a single guest command
> end-to-end. Firecracker HTTP API, snapshots, guest networking, and jailer
> are not yet wired up — see [Known gaps](#known-gaps).

## What works today

- `hephaestus ping` — Rust ⇄ Swift FFI roundtrip.
- `hephaestus rootfs --from-tar X --output Y.ext4` — build a guest ext4
  rootfs from any tar archive (gzip/zstd/none auto-detected).
- `hephaestus run --kernel K --initfs I --rootfs R -- ARGV…` — boot a
  Linux microVM, run the configured process, stream stdout/stderr back to
  the host terminal, exit with the guest's exit code.

## Requirements

- Mac with **Apple silicon**
- **macOS 26** (Tahoe or later)
- **Xcode 26** set as the active developer directory (`xcode-select`)
- **Rust** stable (Homebrew `rust` works)
- [`just`](https://github.com/casey/just) (optional, for the bundled recipes)
- [`apple/container`](https://github.com/apple/container) installed —
  `brew install container`. We reuse its cached kernel + vminit artifacts
  rather than building our own.

## Quickstart

```bash
# 1. One-time: seed apple/container's artifact cache.
container system start
container run --rm docker.io/library/alpine:3.20 echo hi

# 2. Build hephaestus. The linker wrapper at scripts/link-and-sign.sh
#    auto-signs the binary with the com.apple.security.virtualization
#    entitlement on every cargo build.
just build

# 3. Smoke tests.
just ping                # → pong
just verify-signing      # → OK: signed with virtualization entitlement
just artifacts           # → prints discovered kernel/initfs/rootfs paths

# 4. Boot a VM and run a command inside the guest.
just hello               # echoes "hello-from-hephaestus"
just shell               # uname -a; cat /etc/os-release; ls /

# 5. Arbitrary commands. No `--` prefix needed; see `just --list` for more.
just run /bin/cat /etc/hostname

# 6. Tail the kernel boot log from the last run.
just bootlog
```

For shell chains with quoting (`sh -c '...'`), invoke the helper script
directly — `just`'s variadic args drop quotes:

```bash
scripts/run-vm.sh /bin/sh -c 'uname -a; ls /; echo exit=$?'
```

## Architecture

```
┌───────────────────────────────────────────┐
│  hephaestus-cli       (Rust bin)          │  ./build/cargo_target/debug/hephaestus
├───────────────────────────────────────────┤
│  hephaestus-vmm       (Rust lib)          │  high-level VM API
├───────────────────────────────────────────┤
│  hephaestus-bridge    (Rust lib)          │  unsafe FFI + cbindgen header
│  ─── C ABI ──────────────────────────     │
│  HephaestusBridge     (Swift package)     │  @_cdecl exports
├───────────────────────────────────────────┤
│  apple/containerization                   │  Kernel, Mount, LinuxContainer,
│                                           │  VZVirtualMachineManager, vminitd agent
├───────────────────────────────────────────┤
│  Apple Virtualization.framework           │  hypervisor
└───────────────────────────────────────────┘
```

Key bits:

- **Rust ⇄ Swift FFI** via `cbindgen`-generated C header imported by SwiftPM
  through a `CHephaestusBridge` module-map target.
- **Static linking** of the Swift package: `cargo build` shells out to
  `xcrun swift build --triple arm64-apple-macosx15.0`, locates the produced
  `libHephaestusBridge.a`, and force-loads it into the final Rust binary
  (`-Wl,-force_load,<path>`) so Swift module-init sections run and type
  metadata for NIO / swift-atomics / grpc-swift gets registered.
- **Async-to-sync bridging** from the synchronous Rust FFI: `Task.detached`
  on a pthread queue + `DispatchSemaphore` signals completion.
- **Codesigning** happens at the linker step via `scripts/link-and-sign.sh`
  (configured as the macOS linker in `.cargo/config.toml`). Only the final
  `hephaestus` binary gets the VM entitlement; build scripts and
  proc-macros are deliberately skipped to avoid SIGKILL.
- **stdio** from the guest flows back to the host through a Swift
  `Writer` implementation that invokes a Rust `extern "C"` callback.

## Layout

```
src/hephaestus-cli/          Rust bin crate — argv parsing, stdio sink
src/hephaestus-vmm/          Rust lib — re-exports from bridge
src/hephaestus-bridge/       Rust FFI crate — cbindgen, extern declarations
swift/HephaestusBridge/      Swift package — @_cdecl impls, Containerization use
scripts/link-and-sign.sh     Linker wrapper that codesigns the CLI
scripts/run-vm.sh            Artifact discovery + CLI invocation helper
hephaestus.entitlements      com.apple.security.virtualization
justfile                     Dev recipes
```

The original Firecracker workspace crates (`vmm`, `jailer`, `utils`,
`seccompiler`, `firecracker`, …) still sit under `src/` but are excluded
from the macOS workspace build in `Cargo.toml`. They're retained for
upstream cherry-picks.

## Status / known gaps

### What's wired up

- **Guest networking** via VZ's built-in NAT (`VZNATNetworkDeviceAttachment`).
  Fixed `192.168.64.0/24` subnet; gateway `.1`. Per-VM last octet is
  hashed deterministically from the VM id (`allocate_ip_octet`); override
  with `--ip N`. No port-forwarding host→guest yet.
- **Interactive pty** via `--tty` (LinuxContainer path) or `hephaestus vz-sh`
  (direct VZ path).
- **Snapshot save/restore** on the direct-VZ path via
  `hephaestus vz-snapshot save/restore`. Restore+resume ≈ 200 ms on a
  512 MiB VM. Does not yet integrate with the full container
  orchestration path (`hephaestus run`).

### Still missing

- **No stdin** on the `hephaestus run` path. Stdout/stderr stream out;
  stdin isn't wired. Use `--tty` for interactive.
- **No port-forwarding host→guest.** The guest can reach outbound but
  nothing on the host can connect *in* without a separate proxy.
- **Snapshot + container integration.** `hephaestus run` doesn't save
  or restore — that's future work that requires a vminitd-equivalent on
  the direct-VZ path.
- **No Firecracker HTTP API.** The `firecracker` bin crate is excluded
  from the macOS build pending a backend-trait refactor.
- **No jailer.** The upstream `jailer` crate is Linux-only (cgroups +
  namespaces + seccomp). A macOS-native replacement using App Sandbox
  profiles is planned.
- **No rate limiters, MMDS, balloon.** Listed on the roadmap; none are
  blocking for V1's scope.
- **No x86_64 guests.** Apple Virtualization.framework is aarch64-only on
  Apple Silicon; x86_64 code paths were removed rather than stubbed.

### Operational notes

- **Per-VM rootfs cloning.** `scripts/run-vm.sh` `cp -c`'s both the initfs
  and the container rootfs under `$TMPDIR/hephaestus/` keyed on VM id,
  because a single ext4 file can't be attached read-write to two
  concurrent VMs. APFS CoW makes this effectively free. Set
  `HEPHAESTUS_ROOTFS_SHARED=1` / `HEPHAESTUS_INITFS_SHARED=1` to opt out
  (e.g., when you want changes to persist back to the source file).

## Relationship to upstream Firecracker

This repo began as a full git fetch of `firecracker-microvm/firecracker`
then diverged. We keep `upstream` as a git remote for cherry-picking
fixes into shared, OS-agnostic crates (`acpi-tables`, `log-instrument`,
`clippy-tracing`). The Linux-only core (`vmm` + friends) is on ice until
the backend-trait refactor enables a macOS build path.

## Why "hephaestus"?

Hephaestus forged things in a volcano. This project forges lightweight
Linux VMs inside Apple silicon.

## License

Apache 2.0, inherited from upstream Firecracker. See [LICENSE](LICENSE)
and [NOTICE](NOTICE).
