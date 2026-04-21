# hephaestus changelog

The upstream Firecracker changelog is preserved under
[`CHANGELOG-upstream.md`](CHANGELOG-upstream.md). Only changes specific to
the hephaestus fork are tracked here.

The format loosely follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/)
and this project adheres to [Semantic Versioning](https://semver.org/).

## [0.2.0] - 2026-04-21

Second tagged release. Adds process execution and snapshot-based warm
starts on the direct-VZ path, without depending on apple/containerization
or vminitd.

### Added

- `guest/hephaestus-agent` — new out-of-workspace Rust crate cross-
  compiled to `aarch64-unknown-linux-musl` (rustup + `rust-lld`, no
  third-party toolchain). Serves as PID 1
  out of a 184 KB gzipped-cpio initramfs: mounts `/proc`/`/sys`/`/dev`,
  mounts `/dev/vda` at `/newroot`, `chroot`s in, listens on vsock port
  1234, reads a length-prefixed UTF-8 command, `exec`s `/bin/sh -c CMD`,
  writes the guest's exit code as i32 LE back over the same vsock
  connection, and calls `reboot(RB_POWER_OFF)` so the host sees a clean
  `.stopped` state transition.
- `scripts/build-agent.sh` — cross-compile the agent (using rustup's
  `rust-lld` + self-contained musl crt, no third-party cross-toolchain)
  and pack it with `cpio -H newc | gzip` into `build/agent.cpio.gz`.
- `hephaestus vz-exec --cmd CMD --kernel K --rootfs R` — boot the agent
  initramfs, ship the command over vsock (not kernel cmdline — that's
  the key shift from v0.1.0), collect stdout on host stdout and the
  exit code on host exit. Cold wall-clock ≈ 300-500 ms.
- `hephaestus vz-warm save --save PATH …` — pre-warm a VM with the
  agent accepting connections, snapshot it. Pair with
  `hephaestus vz-warm run --save PATH --cmd CMD …` to restore + deliver
  a fresh command to the already-booted agent. Same save file drives
  many commands because the command isn't baked into the snapshot.
  Restore + resume ≈ 200 ms; wall ≈ 340 ms per command.
- `just vz-exec 'CMD'` / `just vz-warm-save` / `just vz-warm-run 'CMD'`
  recipes that auto-discover the kernel + rootfs from apple/container's
  cache and APFS-CoW-clone them per invocation.

### Internals

- Agent's `accept()` loops over probe connections so the host can
  connect-without-writing as a readiness check before snapshotting
  without burning the one-shot command slot.
- Snapshot-compatible VM config uses URL-based
  `VZFileSerialPortAttachment` (FileHandle attachments don't serialize
  across save/restore) and persists the `VZGenericMachineIdentifier` in
  a sibling `.machineid` file so restore rebuilds a structurally
  identical config.
- New `ExecSession` holds VM config + Pipe + log handle together so
  serial-streaming resources outlive the config-builder's return — fixed
  a real ARC bug where the Pipe was being deallocated before the VM
  could write anything.
- Vsock wire protocol between host (Swift) and guest (Rust): u32 LE
  length + UTF-8 command → i32 LE exit code → close. Trivial enough that
  both sides fit in ~40 lines each.
- `.cargo/config.toml` in `guest/hephaestus-agent` sets
  `linker = "rust-lld"` and `-C link-self-contained=yes` so rustup's own
  bundled linker + musl crt handle the cross-compile. No zig, no
  Docker, no musl-cross-toolchain sysroot on the host.

### Requirements added

- `rustup` with the `aarch64-unknown-linux-musl` target for the guest
  agent build (`rustup target add aarch64-unknown-linux-musl`). The main
  host workspace still builds fine with Homebrew `cargo`.

## [0.1.0] - 2026-04-21

Initial tagged release. A macOS / Apple Silicon fork of Firecracker
retargeted at Apple's `Virtualization.framework` via the
[`apple/containerization`](https://github.com/apple/containerization)
Swift package.

### Added — container-backed path (`hephaestus run`)

- `hephaestus run --id ID --kernel K --initfs I --rootfs R -- ARGV…`
  — boot a Linux VM, run a guest process, stream stdout/stderr through
  Rust callback trampolines, exit with the guest's exit code.
- `--network` attaches a NAT-backed virtio-net interface so the guest
  has outbound IPv4 / DNS. Uses VZ's built-in
  `VZNATNetworkDeviceAttachment` so we only need the
  `com.apple.security.virtualization` entitlement — not the restricted
  `com.apple.vm.networking` that's incompatible with ad-hoc signing.
- `--tty` attaches the guest process to the host's controlling
  terminal as a pty (`setTerminalIO`), raw-mode'd for the duration so
  Ctrl-C/Ctrl-D land in the guest.
- `--ip N` (or full `192.168.64.N`) overrides the deterministic IP
  allocation; otherwise we FNV-1a hash the VM id to a unique last
  octet in `[2, 254]`.

### Added — direct `Virtualization.framework` path

Bypasses apple/containerization and drives `VZVirtualMachine` directly.

- `hephaestus vz-boot` — boot a kernel + rootfs, capture guest serial
  to a file, stop after a timeout.
- `hephaestus vz-snapshot save` — pause and save the full machine
  state via `VZVirtualMachine.saveMachineStateTo(url:)`. Produces a
  save file + sibling `.machineid` so restore can rebuild a
  structurally-identical config.
- `hephaestus vz-snapshot restore` — build a matching config, call
  `restoreMachineStateFrom(url:)` + `resume()`, report elapsed time.
  Restore + resume of a 512 MiB guest on an M-series Mac lands at
  **~200 ms** (measured on the default alpine rootfs).
- `hephaestus vz-sh` — interactive serial shell on the direct-VZ path,
  no vminitd involvement. Watches for `Kernel panic` in the serial
  stream to detect `exit` / Ctrl-D and tear the VM down cleanly.

### Added — tooling

- `hephaestus rootfs --from-tar X --output Y.ext4` converts a tar
  archive into an ext4 block device via apple's `EXT4Unpacker`.
  Compression is auto-detected from magic bytes (gzip / zstd / none).
- `justfile` + `scripts/run-vm.sh` — dev recipes (`just ping`,
  `just hello`, `just network-check`, `just sh`, `just vz-sh`,
  `just parallel-net-check`, `just test`, `just test-rootfs`,
  `just bootlog`, `just artifacts`). The runner script auto-discovers
  the kernel + init.ext4 + rootfs paths from apple/container's local
  cache and APFS-clones them per VM id so concurrent runs don't fight
  over a single ext4 file.
- Auto-codesigning of the built binary with the
  `com.apple.security.virtualization` entitlement via a linker wrapper
  at `scripts/link-and-sign.sh`, wired as the macOS linker in
  `.cargo/config.toml`. Filters so only the final `hephaestus` binary
  gets the entitlement — build scripts and proc-macro dylibs are left
  unsigned so macOS's amfid doesn't SIGKILL them at launch.
- Translated VZ error messages: we catch `NSError` in domain
  `VZErrorDomain`, strip the noisy prefix, and append action-oriented
  hints for the common codes (entitlement missing, invalid disk
  image, etc.).

### Added — internals

- Rust ⇄ Swift FFI via `cbindgen` + a SwiftPM module-map target
  (`CHephaestusBridge`) so both sides share identical struct layouts.
- Static-link fix: `-Wl,-force_load` on `libHephaestusBridge.a` via a
  second `build.rs` in the bin crate. Without this, the linker lazily
  loads only objects whose symbols Rust references, and Swift's
  `__DATA,__mod_init_func` sections from transitive SwiftPM deps
  (swift-atomics, NIO, grpc-swift) never run — producing runtime
  crashes in `swift_allocObject` when NIO allocates
  `ManagedAtomic<Bool>`.
- Async-to-sync bridging from the synchronous Rust FFI:
  `DispatchQueue.global().async { Task { … ; sem.signal() } };
  sem.wait()`. `ErrorBox` reference type carries thrown errors out of
  `@Sendable` closures under Swift 6 strict concurrency.
- Swift 6 strict-concurrency compliance throughout the bridge.

### Requirements

- Mac with Apple Silicon
- macOS 26 (Tahoe or later)
- Xcode 26 as the active developer directory
- Rust stable (Homebrew `rust` works)
- [`apple/container`](https://github.com/apple/container) installed
  (`brew install container`) — we reuse its cached kernel + vminit
  artifacts rather than building our own

### Known gaps

- No stdin forwarding on the `run` path outside of `--tty` mode.
- No host → guest port forwarding.
- Snapshot / restore is direct-VZ-path-only; not integrated with
  `hephaestus run` (would require a vminitd-equivalent process-control
  agent on the direct path).
- No Firecracker HTTP API — the upstream `firecracker` bin crate is
  excluded from the macOS build pending a backend-trait refactor.
- No jailer replacement — the upstream Linux-only `jailer` crate is
  gated out; a macOS App Sandbox-based replacement is future work.
- No rate limiters, MMDS, balloon device.
- `aarch64` guests only (Apple Silicon restriction).
