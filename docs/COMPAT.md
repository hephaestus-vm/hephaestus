# Firecracker API compatibility

This is the long version of the
[README's compat table](../README.md#firecracker-api-compat). Per-
endpoint notes, known deviations, and deferred items.

Legend:

- ✓ **Full** — same wire shape, same semantics.
- ⚠︎ **Partial** — wire shape accepted, semantics differ or are noop.
- ✗ **Not supported** — endpoint returns `NotSupported` or 404.
- `(not routed)` — hephaestus doesn't expose this endpoint yet.

## Core lifecycle

### `GET /`

- **Status:** ✓
- Returns `InstanceInfo` with `app_name`, `id`, `state`,
  `vmm_version`. Required-field markers match upstream; the Go SDK's
  strict deserializer round-trips cleanly.

### `GET /machine-config`, `PUT /machine-config`, `PATCH /machine-config`

- **Status:** ✓
- `vcpu_count` and `mem_size_mib` map to Swift defaults when unset
  (2 / 512 per VZ conventions). `cpu_template` field accepted as
  opaque `serde_json::Value` and ignored — Apple Silicon CPU feature
  control isn't client-configurable in VZ.
- `PATCH` pre-boot only; post-boot returns `InvalidState`.

### `PUT /boot-source`

- **Status:** ✓
- `kernel_image_path`, `boot_args`, `initrd_path` honored. On pool
  restore, `boot_args` is ignored (VZ resumes from the cmdline
  encoded at save time; see [ARCHITECTURE.md](ARCHITECTURE.md#match-key)).

### `PUT /drives/{id}`, `PATCH /drives/{id}`

- **Status:** ⚠︎ Partial
- `PUT` fully honored: `drive_id`, `path_on_host`, `is_root_device`,
  `is_read_only`, `cache_type`, `io_engine`.
- `PATCH` swaps `path_on_host` pre-boot only. VZ's
  `VZVirtioBlockDeviceConfiguration` attachments aren't hot-swappable
  the way Linux virtio-blk + io_uring is. Post-boot `PATCH` returns
  `InvalidState`. `firectl` and Kata both patch drives before
  `InstanceStart`, so pre-boot-only covers the real usage. If a
  client needs post-boot drive patching, stop-and-restart is the
  escape hatch.

### `PUT /network-interfaces/{id}`, `PATCH /network-interfaces/{id}`

- **Status:** ⚠︎ Partial
- Accepted + mostly noop. hephaestus attaches VZ's built-in NAT
  (192.168.64.0/24) to every VM regardless of client config. We
  don't honor MAC address, host tap name, or rate-limiter settings.
- `PATCH` is an accept-noop so `firectl` and Kata don't trip on
  rate-limiter updates.

### `PUT /actions`

- **Status:** ✓
- Only `action_type: "InstanceStart"` is supported. Other actions
  (`SendCtrlAltDel`, `FlushMetrics`) return `NotSupported`.

### `PATCH /vm`

- **Status:** ✓
- `{"state": "Paused"}` and `{"state": "Resumed"}` map to
  `VzVm::pause` / `VzVm::resume`. Enforces `Running ↔ Paused`
  transitions; idempotent. Bogus values rejected with serde's
  variant error.

## Observability

### `PUT /logger`

- **Status:** ✓
- Opens `log_path` append-mode and writes a structured init line.
  Good enough for clients that only check the file exists + grows.
  Full `log` crate plumbing is deferred — won't break existing
  clients when it lands.

### `PUT /metrics`

- **Status:** ⚠︎ Partial
- Writes an init line to `metrics_path`. Periodic flush of upstream's
  ~30 metric fields is deferred — most don't map to macOS primitives
  (KVM exit counters etc.). Won't be byte-for-byte compatible with
  real Firecracker's metrics log even when we ship the periodic
  writer.

### `GET /version`

- **Status:** ✗ (not routed)
- Deferred. The Go SDK exposes `GetFirecrackerVersion` but `firectl`
  doesn't issue it during normal startup. Add when something breaks
  without it.

## Snapshots

### `PUT /snapshot/create`

- **Status:** ✓ (with caveats)
- Requires `Paused` state. Wire accepts `snapshot_path` +
  `mem_file_path`; hephaestus writes the full VZ save blob to
  `snapshot_path` and touches an empty stub at `mem_file_path`
  ("A+stub"). VZ's `saveMachineStateTo:` produces one combined
  blob — we can't split it.
- Rejects `snapshot_type: Diff` (VZ has no dirty-page tracking).
- Rejects pool-restored VMs: the load path (`vz_long_restore`) uses
  `buildLongRunningConfig` which produces a no-vsock no-initramfs
  config, and pool VMs were built with `ExecSession.makeSnapshotable`
  (vsock + initramfs). Restoring a pool-flavor save with the loader
  fails VZ's "configuration mismatch" check, so we reject at save
  time. Workaround: stop the pool VM and cold-boot a new one before
  snapshotting.

### `PUT /snapshot/load`

- **Status:** ✓
- Requires `NotStarted` + all pre-boot config supplied (kernel,
  rootfs, vcpu_count, mem_size_mib, optional initrd). `resume_vm:
  true` (default) transitions to `Running`; `false` to `Paused`.
- `mem_backend.backend_type: "Uffd"` returns `NotSupported` (Linux-
  only userfaultfd).
- `enable_diff_snapshots` / `track_dirty_pages` return `NotSupported`.

### Cross-tool interop

Real Firecracker's save blob format and VZ's `saveMachineStateTo:`
format are both hypervisor-specific. A blob produced by one *cannot*
be loaded by the other — this is fundamental, not a hephaestus bug.
hephaestus scopes drop-in compat to "save in hephaestus, load in
hephaestus", which is the realistic use case (fast restart + live
migration between hephaestus processes).

## Not routed / deferred

- **`PUT /mmds`, `GET /mmds`, `PATCH /mmds`** — metadata service.
  Deferred indefinitely; hephaestus guests can get IMDS-style data
  through simpler means.
- **`PUT /vsock`** — beyond our agent's fixed port 1234, we don't
  expose client-configurable vsock.
- **`PUT /balloon`, `PATCH /balloon`, `PATCH /balloon/statistics`**
  — VZ doesn't expose a balloon device.
- **`PUT /entropy`** — no virtio-rng configurable device.
- **`PUT /cpu-config`, CPU templates** — field accepted as opaque
  JSON and ignored.
- **`PATCH /vm` with anything other than Paused/Resumed** — upstream
  has `RestoreVm`/`CreateSnapshot`/etc.; hephaestus uses the
  dedicated `/snapshot/*` endpoints instead.

## Regression harness

`compat/firectl-harness/` is a ~250-line Go binary that drives our
server through `firecracker-go-sdk`'s swagger-generated client —
same marshaling + strict deserializer that real `firectl` and Kata
use. Run after every upstream rebase:

```bash
just fc-compat            # cold boot (14 SDK calls)
just fc-compat-pool       # warm-pool restore (agent flavor)
just fc-compat-pool-stock # warm-pool restore (stock-init flavor)
just fc-compat-snapshot   # save/stop/fresh-process/load round-trip
```

All four expected to return 14/14 after any wire-shape change.
