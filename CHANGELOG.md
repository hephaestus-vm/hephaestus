# Changelog

All notable changes to hephaestus. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); version
numbers follow [Semantic Versioning](https://semver.org/).

## [0.4.0-alpha.3](https://github.com/hephaestus-vm/hephaestus/compare/v0.4.0-alpha.2...v0.4.0-alpha.3) (2026-07-20)


### Features

* **networking:** serve transparent MMDS over authorized vmnet ([#22](https://github.com/hephaestus-vm/hephaestus/issues/22)) ([5025492](https://github.com/hephaestus-vm/hephaestus/commit/502549285dfd07d2676b766b9b49612f003cd51d))


### Bug Fixes

* **release:** tolerate no-op automation runs ([#28](https://github.com/hephaestus-vm/hephaestus/issues/28)) ([d12420f](https://github.com/hephaestus-vm/hephaestus/commit/d12420fa4a4fcc6e164d2d267182713e7b340e10))
* **release:** use the generated pull request output ([#24](https://github.com/hephaestus-vm/hephaestus/issues/24)) ([23ebc59](https://github.com/hephaestus-vm/hephaestus/commit/23ebc59fad45ff7a8d29fd499beef75068948c06))

## [0.4.0-alpha.2](https://github.com/hephaestus-vm/hephaestus/compare/v0.4.0-alpha.1...v0.4.0-alpha.2) (2026-07-13)


### Features

* **install:** add verified binary installer ([#20](https://github.com/hephaestus-vm/hephaestus/issues/20)) ([321b298](https://github.com/hephaestus-vm/hephaestus/commit/321b298067c8626593aca7a84ac47a6106354a76))


### Bug Fixes

* **release:** detect the bootstrap release ([f0fad1b](https://github.com/hephaestus-vm/hephaestus/commit/f0fad1b4c589061b15767fca3cce94ca77e3426d))

## [Unreleased]

### Added

- **Profile-authorized vmnet networking.** `just probe-vmnet` and
  `just sign-vmnet` now build app bundles with an embedded provisioning profile
  instead of placing a restricted entitlement on a standalone executable.
  `hephaestus-firecracker --network-backend vmnet` attaches configured guest
  NICs to a process-owned `VMNET_SHARED_MODE` network through
  `VZVmnetNetworkDeviceAttachment`; NAT remains the default. With
  `--host-mmds`, a root-free raw Ethernet/TCP responder claims
  `169.254.169.254` inside that network, making MMDS reachable from stock guest
  images without the agent shim or host route/interface changes.

## [0.4.0-alpha.1] — 2026-07-12

### Added

- **Conventional nested CLI help.** `hephaestus`, every top-level command, and
  nested `pool`, `vz-warm`, and `vz-snapshot` commands now accept `-h` and
  `--help`; `-V`/`--version` reports the CLI version.
- **`vz-exec --stdin` forwards host stdin to the guest command.** The
  CLI wraps `--cmd` with a `__hephaestus_stdin__` sentinel; the guest
  agent strips it and pumps vsock bytes into the child's stdin. The
  Swift FFI gains a `forward_stdin` flag on `hb_vz_exec` and a
  background pump thread (`pumpStdinToVsock`) that copies host stdin →
  vsock until EOF. Lets `vz-exec` cover interactive command delivery
  without keeping `vz-sh` for that use case.
- **Separate stdout/stderr streams in `vz-exec`.** A second
  `VZVirtioConsoleDeviceSerialPortConfiguration` (hvc1) carries guest
  stderr to the host's stderr; the agent `dup2`s `/dev/hvc1` onto fd 2
  before exec'ing the child. Cold boot live-streams hvc1 to host fd 2;
  best-effort there means a missing `/dev/hvc1` leaves stderr merged on
  hvc0. The snapshot/restore path (`vz-warm`) now also carries a second
  hvc1 serial, but URL-based so it survives restore — stderr lands in a
  sibling `<log>.stderr` file rather than the host's fd 2 (a restored VM
  can't live-stream). NOTE: this adds a serial port to the saved VM
  config, so `vz-warm` snapshots taken before this change can no longer
  be restored (regenerate them with `vz-warm save`).
- **`hephaestus-firecracker --host-mmds` scaffold.** Binds a host-side
  HTTP listener on `169.254.169.254:80` that serves the current MMDS JSON
  with Firecracker-style path-aware semantics (JSON subtrees for
  `Accept: application/json`, IMDS-style plain text otherwise). Real-VM
  reachability still requires `VZVmnetNetworkDeviceAttachment` +
  `com.apple.vm.networking` + a Developer ID signed binary; the
  listener binds and the handler runs even without those, so the
  scaffold is shippable behind the flag until the entitlement story is
  ready.
- **`hephaestus-jailer` binary.** Per-VM supervisor that materializes a
  per-VM work dir, canonicalizes caller-supplied kernel/rootfs/initramfs
  paths, generates a deny-by-default macOS sandbox profile granting
  only those paths plus the work dir subtree, and execs
  `hephaestus-firecracker` under that profile. A Rust port of the
  existing `scripts/generate-fc-sandbox-profile.sh` lives in the
  `profile` module so the same generation logic is available
  programmatically. Not a launchd job, not a full Firecracker jailer
  replacement — no uid/gid drop, no chroot, no cgroup pinning. The
  sandbox profile is the only isolation boundary.

### Changed

- **User guides are binary-first.** Operator workflows now document
  `hephaestus`, `hephaestus-firecracker`, and `hephaestus-jailer` commands
  directly, with `just` recipes identified as source-tree shortcuts or test
  harnesses.

### Fixed

- **Xcode 27 beta SPM regression with header-only C target.** Added
  `swift/HephaestusBridge/Sources/CHephaestusBridge/dummy.c` (empty
  translation unit) so SPM emits `CHephaestusBridge.o` for the
  header-only C target — without it, Xcode 27 beta skips the compile
  phase and the downstream `libtool` step fails looking for the `.o`.
- **`hephaestus-firecracker` runtime linking on Xcode 27 beta.**
  `hephaestus-firecracker` lacked a `build.rs`, so cargo didn't emit the
  `-Wl,-rpath,/usr/lib/swift` flag the cli's build.rs emits — and the
  new `libswiftCompatibilitySpan.dylib` strong reference Xcode 27 emits
  needs that rpath to resolve at runtime. Added a `build.rs` mirroring
  `hephaestus-cli`'s, plus a direct `hephaestus-bridge` dependency so
  `DEP_HEPHAESTUS_BRIDGE_NATIVE_ARCHIVE` propagates and the archive can
  be force-loaded. Without this, every freshly-built
  `hephaestus-firecracker` (including test binaries) failed with
  `dyld: Library not loaded: @rpath/libswiftCompatibilitySpan.dylib`.

## [0.3.0-alpha.1] — 2026-04-22

First public release. Adds a drop-in Firecracker HTTP API over UNIX
socket, warm-pool integration, snapshot endpoints, and per-phase
restore instrumentation. 14/14 Go-SDK calls round-trip cleanly
against our server on macOS.

### Added

- **`hephaestus-firecracker` binary** — tokio + hyper HTTP/1.1
  server on a UNIX socket. Implements the Firecracker HTTP API
  surface that `firectl`, `firecracker-go-sdk`, and Kata actually
  hit: `GET /`, `GET/PUT/PATCH /machine-config`, `PUT /boot-source`,
  `PUT/PATCH /drives/{id}`, `PUT/PATCH /network-interfaces/{id}`,
  `PUT /logger`, `PUT /metrics`, `PUT /actions`, `PATCH /vm`,
  `PUT /snapshot/create`, `PUT /snapshot/load`. All use
  Firecracker-shape `{"fault_message": ...}` error bodies; 204 on
  PUT/PATCH success, 200+JSON on GET. `deny_unknown_fields` enforced.
- **`hephaestus-fc-api` crate** — pure-serde wire types copied from
  upstream Firecracker's `src/vmm/src/vmm_config/*` (now under
  `vendor/firecracker/`). Per-file `// upstream:` pointers make
  rebase-diffs mechanical.
- **Warm-pool HTTP integration.** `hephaestus-firecracker --pool-dir`
  restores from a matching pool slot on `InstanceStart` when the
  client config matches on `(kernel, rootfs, vcpu_count,
  memory_mib)`; falls back silently to cold boot on any miss.
- **Stock-init pool flavor** (`pool init --stock-init`). Pool
  snapshots can now be taken with the rootfs's own `/bin/sh` as
  PID 1 — no agent, no vsock, no initramfs — so restored VMs are
  behaviorally indistinguishable from cold-boot VMs for HTTP-API
  consumers.
- **Snapshot endpoints** `PUT /snapshot/create` and
  `PUT /snapshot/load`. A+stub: the real blob lives at
  `snapshot_path`; `mem_file_path` is a touched stub so clients'
  existence checks pass. Save/stop/fresh-process/load round-trip
  restores in ~235 ms.
- **Per-phase restore timings.** `HbRestoreTimings` C struct
  (`config_nanos`, `construct_nanos`, `restore_nanos`,
  `resume_nanos`) filled in by every VZ restore FFI; pool layer
  adds `clone_nanos`. HTTP backend logs the full breakdown on
  every `InstanceStart`/`snapshot/load`. Confirmed `restore` phase
  alone owns ~90 % of wall time across all paths.
- **`compat/firectl-harness/`** — Go binary that drives our server
  through `firecracker-go-sdk`'s swagger-generated client. Replays
  the `firectl` call sequence; runs via `just fc-compat`. First-run
  result was 13/13, currently 14/14 after `PUT /metrics` coverage
  was added.
- **Justfile recipes:** `fc-compat`, `fc-compat-pool`,
  `fc-compat-pool-stock`, `fc-compat-snapshot`.

### Fixed

- **`scripts/link-and-sign.sh` multi-word crate glob.** Cargo
  normalizes `-` to `_` in the `deps/<crate>-<hash>` filename for
  multi-word crates, so a glob that only matched `hephaestus-*`
  silently missed `hephaestus_firecracker-*` and shipped the binary
  without the virtualization entitlement. Glob now matches both
  spellings.

### Changed

- **Repo layout.** Upstream Firecracker Linux-only crates relocated
  from `src/` to `vendor/firecracker/` so the hephaestus workspace
  is the only thing in `src/`. Reference material, not built on
  macOS. See `vendor/firecracker/README.md`.
- **Upstream Firecracker docs, tools, tests, and CI removed.**
  The fork now carries only hephaestus-native meta files. Upstream
  is reachable via a git remote for rebase workflows.

## [0.2.0] — 2026-04-21 (local-only tag)

Direct-VZ path: `vz-boot`, `vz-snapshot save/restore`, `vz-sh`,
`vz-exec`, `vz-warm save/run`, `pool init/run/stats/destroy`. Guest
agent (`guest/hephaestus-agent`) cross-compiled to
`aarch64-unknown-linux-musl`. Disk-persistent warm pool over
`flock(2)`. First working snapshot-restore numbers: ~200 ms.

## [0.1.0] — 2026-04-20 (local-only tag)

Containerization-backed foundation: `hephaestus run` boots a VM via
apple/containerization's `LinuxContainer`; `hephaestus rootfs` builds
ext4 block devices from tar archives; `--network` uses VZ NAT,
`--tty` wires a pty.

[Unreleased]: https://github.com/hephaestus-vm/hephaestus/compare/v0.4.0-alpha.1...HEAD
[0.4.0-alpha.1]: https://github.com/hephaestus-vm/hephaestus/releases/tag/v0.4.0-alpha.1
[0.3.0-alpha.1]: https://github.com/hephaestus-vm/hephaestus/releases/tag/v0.3.0-alpha.1
