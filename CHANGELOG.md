# Changelog

All notable changes to hephaestus. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); version
numbers follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

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
  before exec'ing the child. Best-effort: a missing `/dev/hvc1` (older
  config, snapshot-restore path that uses URL serial attachments only)
  leaves stderr on hvc0 and the streams stay merged — same as before.
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

[Unreleased]: https://github.com/hephaestus-vm/hephaestus/compare/v0.3.0-alpha.1...HEAD
[0.3.0-alpha.1]: https://github.com/hephaestus-vm/hephaestus/releases/tag/v0.3.0-alpha.1
