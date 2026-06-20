# Architecture

This is the long-form version of the
[README's architecture section](../README.md#architecture). It walks
the crate layout, the Rust↔Swift FFI contract, the two VM paths, and
the pool flavors that sit underneath the HTTP API.

## Crate layout

```
src/
├── hephaestus-cli/           ← `hephaestus` binary (run/rootfs/vz-*/pool)
├── hephaestus-firecracker/   ← `hephaestus-firecracker` HTTP daemon
├── hephaestus-fc-api/        ← wire types + VmmBackend trait
├── hephaestus-jailer/        ← per-VM supervisor: generates sandbox
│                              profiles and execs hephaestus-firecracker
├── hephaestus-pool/          ← disk-persistent warm-pool primitive
├── hephaestus-vmm/           ← thin re-export over hephaestus-bridge
└── hephaestus-bridge/        ← Rust↔Swift FFI bindings
swift/
└── HephaestusBridge/         ← SwiftPM package — VZ config + FFI impl
guest/
└── hephaestus-agent/         ← Rust agent cross-compiled to
                                 aarch64-unknown-linux-musl, used as
                                 PID 1 for the agent-flavor pool
compat/
└── firectl-harness/          ← Go binary: firecracker-go-sdk smoke
vendor/
└── firecracker/              ← upstream reference (not built on macOS)
```

The workspace has seven members: the hephaestus crates. Everything under
`vendor/firecracker/` is excluded — see
[vendor/firecracker/README.md](../vendor/firecracker/README.md).

## The two VM paths

hephaestus ships two ways to run a VM, because they trade off
differently:

### Containerization path (`hephaestus run`)

Backed by [`apple/containerization`][containerization]'s
`LinuxContainer`. Gets rich process management, stdout/stderr
streaming, pty wiring, and network attachment for free. Does **not**
support snapshots (containerization doesn't expose
`saveMachineStateTo:`), so the HTTP API and pool don't use this path.

### Direct-VZ path (`hephaestus vz-*`, `hephaestus-firecracker`)

We wrote our own Swift VM config (`swift/HephaestusBridge/Sources/HephaestusBridge/DirectVZ.swift`)
directly against `VZVirtualMachine`. This gives us
`saveMachineStateTo:` / `restoreMachineStateFrom:`, which unlocks
snapshots, warm pools, and the HTTP snapshot endpoints.

The cost is reimplementing process delivery. We ship a tiny guest
agent (~200 KB static binary in
[`guest/hephaestus-agent/`](../guest/hephaestus-agent/)) that boots
as PID 1 from a gzipped-cpio initramfs, mounts the rootfs, chroots,
listens on vsock port 1234, reads a length-prefixed command, execs
`/bin/sh -c CMD`, writes the exit code back over vsock, and
`reboot(RB_POWER_OFF)`s.

[containerization]: https://github.com/apple/containerization

## Rust ⇄ Swift FFI

The bridge is `@_cdecl` Swift functions on the other side of a
cbindgen-generated C header. Rust declares `unsafe extern "C"` blocks
matching each symbol; each call goes through a single status enum +
out-param pattern.

- **Header generation:** `src/hephaestus-bridge/build.rs` runs cbindgen
  on every build, writing
  `swift/HephaestusBridge/Sources/CHephaestusBridge/include/hephaestus_bridge.h`.
- **Swift build:** same `build.rs` invokes `xcrun swift build -c
  release --triple arm64-apple-macosx15.0`. The resulting static
  archive links into every Rust binary.
- **Load-bearing linker flag:** `-Wl,-force_load` on the Swift
  archive. Without it, SwiftNIO's type metadata doesn't register and
  allocation crashes at startup.

Opaque types on the C boundary: `HbVm` (containerization path),
`HbVzVm` (direct-VZ long-running VM handle), `HbRestoreTimings` (per-
phase restore instrumentation — see [perf.md](perf.md)). The long-running
handle also exposes `hb_vz_long_connect`, which dup(2)s a host-side fd for
`VZVirtioSocketDevice.connect(toPort:)`; `hephaestus-firecracker` uses that
for Firecracker-style `PUT /vsock` UDS bridging. See
`src/hephaestus-bridge/src/lib.rs` top for the full list.

## `VzBackend` state machine

`hephaestus-firecracker`'s `VzBackend`
(`src/hephaestus-firecracker/src/backend.rs`) implements the
`VmmBackend` trait defined in `hephaestus-fc-api`. One backend per
process; one VM per backend (matching upstream's contract).

States: `NotStarted → Running ⇄ Paused`. `PUT /snapshot/load` goes
`NotStarted → Running` (or `Paused` if `resume_vm: false`);
`PUT /snapshot/create` requires `Paused`.

`RunOrigin` tracks how the current VM started — `ColdBoot`, `Pool`,
or `SnapshotLoad` — because `create_snapshot` rejects `Pool` (the
restore loader can't reproduce the pool-flavor config).

## Warm-pool flavors

`hephaestus-pool`'s `PoolFlavor` enum has two values:

### Agent

Used by the CLI's `pool run` (command injection via vsock). Snapshot
taken with our agent as PID 1; `/bin/sh` only runs after the agent
delivers a command. The HTTP backend *also* uses agent-flavor pools
by default, but never connects to the agent — the VM just sits at
`accept()` forever and looks like a running instance to the HTTP
client.

### StockInit

Snapshot taken with the rootfs's own `/bin/sh` as PID 1 (no agent,
no vsock, no initramfs). HTTP consumers see a VM that's
behaviorally identical to cold boot. `Pool::run` refuses StockInit
pools (no command channel). `pool init --stock-init` selects it.

Both flavors are restored via `Pool::restore_into_vm` which dispatches
on `PoolMeta.flavor` and returns a unified `(VzVm,
PoolRestoreBreakdown)`. The HTTP backend doesn't care which path it got.

## Guest-visible MMDS

The Firecracker control-plane MMDS endpoints store JSON in `VzBackend`.
For direct-VZ VMs, hephaestus exposes that JSON inside the guest through a
reserved virtio-vsock service on port `16992` (port `1234` stays reserved for
`hephaestus-agent`). The Swift bridge installs a `VZVirtioSocketListener` via
`hb_vz_long_serve_mmds`; each guest connection gets an HTTP/1.1 JSON response
with the current MMDS document. Controlled agent-flavor e2e guests can also
exercise a guest-side `169.254.169.254:80` shim that forwards to this vsock
service. `hephaestus-firecracker --host-mmds` can additionally bind a host-side
`169.254.169.254:80` listener with the same path-aware semantics, but that path
is scaffolded until vmnet attachment + `com.apple.vm.networking` signing are in
place for real guest reachability; see [JAILER_MMDS_PLAN.md](JAILER_MMDS_PLAN.md).

## Match key

`PoolMatchSpec` is `(canonical(kernel), canonical(rootfs),
vcpu_count, memory_mib)`. Boot-args are deliberately omitted from the
match key: VZ's `restoreMachineStateFrom:` resumes from saved kernel
state, and the cmdline encoded in the snapshot's bootloader config
is what the guest sees. A client-supplied `boot_args` can't take
effect on a restored VM, so comparing it would only shrink the hit
rate.

## Restore timing

Every VZ restore FFI fills in an `HbRestoreTimings` struct with four
phases:

- `config_nanos` — config object construction (path canonicalization,
  attachment setup, machine-identifier load).
- `construct_nanos` — `VZVirtualMachine(configuration:queue:)`.
- `restore_nanos` — `restoreMachineStateFrom:`.
- `resume_nanos` — `resume()`.

Pool layer wraps that in `PoolRestoreBreakdown` with an extra
`clone_nanos` for the `cp -c` rootfs clone. HTTP backend logs the
full line on every restore. See [perf.md](perf.md) for the numbers.

## Further reading

- [vendor/firecracker/README.md](../vendor/firecracker/README.md) —
  why the upstream tree is here and how to re-sync.
- [docs/COMPAT.md](COMPAT.md) — per-endpoint Firecracker API status.
- [docs/perf.md](perf.md) — restore-timing methodology + raw numbers.
