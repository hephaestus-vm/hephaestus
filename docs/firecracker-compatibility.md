# Firecracker API compatibility

This is the canonical record of the Firecracker API version, endpoint
support, known deviations, and regression coverage. The
[README](../README.md#capabilities-and-limitations) contains only a summary.

Status vocabulary:

- **Supported** — compatible wire shape and semantics.
- **Supported with differences** — the operation works, but documented
  semantics differ.
- **Accepted but ignored** — the wire shape is accepted for client
  interoperability, but the setting has no effect.
- **Unsupported** — the endpoint or option returns `NotSupported` or is not
  routed.
- **Not applicable** — the concept has no Virtualization.framework equivalent.

The current wire target is **Firecracker 1.16.0-dev**, as reported by
`GET /version`. A status applies only to the behavior described below; fields
called out as ignored or unsupported are not covered by an endpoint's broader
status.

## Core lifecycle

### `GET /`

- **Status:** **Supported**
- Returns `InstanceInfo` with `app_name`, `id`, `state`,
  `vmm_version`. Required-field markers match upstream; the Go SDK's
  strict deserializer round-trips cleanly.

### `GET /machine-config`, `PUT /machine-config`, `PATCH /machine-config`

- **Status:** **Supported**
- `vcpu_count` and `mem_size_mib` map to Swift defaults when unset
  (2 / 512 per VZ conventions). `vcpu_count` must be in `1..=32`
  (Firecracker's `MAX_SUPPORTED_VCPUS`); out-of-range values are
  rejected with `InvalidConfig` like upstream. `cpu_template` is
  accepted by the serde wire layer but rejected with `NotSupported`
  when present — Apple Silicon CPU feature control isn't
  client-configurable in VZ.
- `PATCH` pre-boot only; post-boot returns `InvalidState`.

### `PUT /boot-source`

- **Status:** **Supported**
- `kernel_image_path`, `boot_args`, `initrd_path` honored. On pool
  restore, `boot_args` is ignored (VZ resumes from the cmdline
  encoded at save time; see [Warm pools](guides/warm-pools.md#http-api-integration)).

### `PUT /drives/{id}`, `PATCH /drives/{id}`

- **Status:** **Supported with differences**
- **Root + secondary drives supported.** The root device
  (`is_root_device: true`) boots as `/dev/vda`; additional drives
  (`is_root_device: false`) attach in insertion order as `/dev/vdb`,
  `/dev/vdc`, … Re-`PUT` of an existing `drive_id` updates it in place.
- Honored fields: `drive_id`, `path_on_host`, `is_root_device`, and
  `is_read_only` (per-drive; a read-only drive is attached read-only,
  so the guest cannot mutate it).
- Accepted but **ignored**: `cache_type`, `io_engine`, `partuuid`,
  `rate_limiter`. VZ's built-in block attachment doesn't expose these
  knobs.
- The warm pool serves single-rootfs VMs only, so configuring a
  secondary drive skips the pool fast-path and cold-boots.
- `PATCH` swaps `path_on_host` pre-boot only (by `drive_id`). VZ's
  `VZVirtioBlockDeviceConfiguration` attachments aren't hot-swappable
  the way Linux virtio-blk + io_uring is. Post-boot `PATCH` returns
  `InvalidState`. `firectl` and Kata both patch drives before
  `InstanceStart`, so pre-boot-only covers the real usage. If a
  client needs post-boot drive patching, stop-and-restart is the
  escape hatch.
- Verified by a real-VM smoke: a secondary drive appears as
  `/dev/vdb` in the guest.

### `PUT /network-interfaces/{id}`, `PATCH /network-interfaces/{id}`

- **Status:** **Supported with differences** — NIC attached, L3 config is the guest's job.
- `PUT /network-interfaces/{id}` on the HTTP API path attaches a guest NIC
  backed by VZ's built-in NAT by default (`192.168.64.0/24`, gateway `.1`).
  NAT only needs the base `com.apple.security.virtualization` entitlement, so
  it works under ad-hoc signing. A profile-authorized daemon launched with
  `--network-backend vmnet` instead uses a process-owned shared-mode
  `VZVmnetNetworkDeviceAttachment`. `guest_mac` is honored in either mode (VZ
  assigns a random MAC when omitted).
- Like Firecracker, hephaestus provides the *device*; the guest
  configures L3 (DHCP / `ip=` boot arg / cloud-init). VZ NAT runs a
  DHCP server, so a guest with a DHCP client gets a `192.168.64.0/24`
  lease automatically.
- **Ignored**: `host_dev_name` (VZ manages the host side), and
  `rx_rate_limiter` / `tx_rate_limiter` (VZ exposes no I/O rate knob).
- Guest-visible link-local MMDS (`169.254.169.254`) rides the
  `hephaestus-agent` shim in the default NAT mode because VZ NAT is a black box.
  A profile-authorized daemon started with `--network-backend vmnet
  --host-mmds` instead answers ARP and serves MMDS directly on the VM's virtual
  Ethernet segment, so stock images do not need the shim.
- `PATCH` is an accept-noop (VZ attachments aren't hot-swappable).
- Verified by `just fc-compat-net-e2e` (boots with a NIC, confirms a
  non-loopback netdev appears in the guest).

### `PUT /actions`

- **Status:** **Supported**
- `action_type: "InstanceStart"` cold-boots/restores the VM.
- `action_type: "FlushMetrics"` forces an immediate metrics JSON flush when
  `PUT /metrics` has configured a sink.
- `action_type: "SendCtrlAltDel"` requests a graceful guest shutdown via
  VZ `requestStop()` (ACPI-style stop). The guest powers off rather
  than reboots — the closest VZ analog — and it's only valid while the
  VM is `Running`, matching upstream.

### `PATCH /vm`

- **Status:** **Supported**
- `{"state": "Paused"}` and `{"state": "Resumed"}` map to
  `VzVm::pause` / `VzVm::resume`. Enforces `Running ↔ Paused`
  transitions; idempotent. Bogus values rejected with serde's
  variant error.

## Observability

### `PUT /logger`

- **Status:** **Supported**
- Opens `log_path` append-mode and emits Firecracker-style text records:
  `<timestamp> [<instance-id>:<thread>[:LEVEL][:origin:line]] <message>`.
  The backend honors `level`, `show_level`, `show_log_origin`, and a
  prefix-style `module` filter where practical. Lifecycle events log at
  info; HTTP access records include `request_id`, method, path, and status
  and are emitted only when the configured level enables `Debug`.

### `PUT /metrics`

- **Status:** **Supported with differences**
- Opens `metrics_path` append-mode (handle held for the process lifetime,
  not reopened per request) and writes newline-delimited JSON. Flushes
  happen at configure time, after each API request/lifecycle event, and
  from a 60s background timer (matching Firecracker's default cadence).
  The shape includes Firecracker-compatible top-level groups: `api_server`,
  `get_api_requests`, `put_api_requests`, `patch_api_requests`, `logger`,
  `vmm`, `vcpu`, and `seccomp`. The **`*_api_requests` counters are real**
  — each served request is classified by `(method, path)` into the matching
  per-endpoint counter. Device/hypervisor counters with no VZ equivalent
  (`vmm.panic_count`, `vcpu.*`, `seccomp.num_faults`, `logger.missed_*`) are
  emitted as zeros. macOS/hephaestus-specific counters live under the
  `hephaestus` object (`api_requests`, failures, pool hits/misses, snapshot
  loads). It is intentionally not byte-for-byte identical to Firecracker's
  full metrics set because most device counters do not exist in VZ.

### `GET /version`

- **Status:** **Supported**
- Returns Firecracker's wire shape:
  `{"firecracker_version":"1.16.0-dev"}`. The value is a pinned
  compatibility target matching the vendored upstream API snapshot,
  not the hephaestus crate version. Bump it only when the wire structs
  have been re-synced and the compat harness passes.

## Snapshots

### `PUT /snapshot/create`

- **Status:** **Supported with differences**
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

- **Status:** **Supported**
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

## Partial / unsupported device surfaces

- **`PUT /mmds`, `GET /mmds`, `PATCH /mmds`, `PUT /mmds/config`** —
  **Supported with differences.** hephaestus stores and returns arbitrary JSON, including a
  recursive merge-patch-style `PATCH`, so orchestrators can configure
  metadata without hitting 404s. On direct-VZ long-running VMs, the current
  MMDS JSON is also served to guest-initiated vsock connections on reserved
  port `16992` as an HTTP/1.1 JSON response. This is practical guest-visible
  metadata. The bundled `hephaestus-agent` starts a guest-side link-local
  MMDS shim by default: it configures `169.254.169.254/32` on loopback, listens
  on `http://169.254.169.254/`, and forwards requests to the vsock MMDS service.
  The vsock service handles Firecracker-style GET paths into the JSON data store:
  `Accept: application/json` returns JSON subtrees, while the default/plain-text
  response lists object keys or returns string leaves in IMDS format. Custom
  images can launch the same helper with `hephaestus-agent mmds-shim`,
  or disable the PID1 auto-start with `HEPHAESTUS_MMDS_SHIM=0` /
  `hephaestus.mmds=off`. The MMDS config's interface binding/version are
  stored but not enforced. If an `ipv4_address` is supplied it must be in
  Firecracker's link-local `169.254.0.0/16` range. `hephaestus-firecracker
  --host-mmds --network-backend vmnet` serves the same path-aware semantics
  directly at `169.254.169.254:80` through a raw vmnet packet interface. It
  answers guest ARP and terminates TCP in user space, requiring neither root
  nor host network changes. `just fc-compat-vmnet-e2e` verifies DHCP plus a
  real guest HTTP fetch without the agent MMDS shim.
- **`PUT /vsock`** — **Supported with differences.** Accepts Firecracker's `guest_cid`,
  `uds_path`, and deprecated `vsock_id` fields pre-boot. hephaestus stores
  `guest_cid` for wire compatibility but VZ assigns the actual CID. After
  the VM starts, hephaestus binds `uds_path`; host clients connect and send
  Firecracker's `CONNECT <guest_port>\n` line, then the stream is bridged to
  `VZVirtioSocketDevice.connect(toPort:)`. Port 1234 remains reserved for
  hephaestus-agent by convention. Config-only CI validates the wire shape;
  `just fc-compat-vsock-e2e` validates the agent/MMDS path, the guest-side
  link-local MMDS shim, and a generic guest-port echo server.
- **`PUT /balloon`, `PATCH /balloon`, `GET /balloon`** — **Supported with differences**
  via VZ's traditional memory balloon. `PUT` (pre-boot) configures the
  reclaim target; `PATCH` live-adjusts it on a running VM. hephaestus
  maps Firecracker's `amount_mib` (memory to reclaim) onto VZ's
  `targetVirtualMachineMemorySize` (memory the guest keeps), so a
  reclaim of `amount_mib` targets `mem_size_mib - amount_mib`.
  `amount_mib` must be `< mem_size_mib` (validated at boot). Actual
  reclaim depends on the guest's virtio-balloon driver. `deflate_on_oom`
  is accepted but managed by VZ; a non-zero `stats_polling_interval_s`
  is rejected.
- **`GET/PATCH /balloon/statistics`, `PATCH /balloon/hinting/start`,
  `GET /balloon/hinting/status`, `PATCH /balloon/hinting/stop`** —
  routed but return `NotSupported`; VZ exposes no balloon statistics or
  free-page hinting.
- **`PUT /entropy`** — **Supported** (pre-boot). The direct-VZ config
  always attaches a virtio-rng (`VZVirtioEntropyDeviceConfiguration`),
  so the guest always has an entropy source; the request is accepted
  and confirmed. Any `rate_limiter` is ignored (VZ exposes no rng rate
  knob).
- **`PUT/PATCH /cpu-config`, CPU templates** — routed/rejected with
  `NotSupported`; Apple Silicon CPU templates are not configurable.
- **`PUT /pmem/{id}`** — routed but returns `NotSupported`; VZ's direct
  Linux path does not expose Firecracker's persistent-memory device model.
- **`PUT /serial`** — routed but returns `NotSupported`; hephaestus owns
  the serial console plumbing for boot logs and agent init.
- **`GET/PUT/PATCH /hotplug/memory`** — routed but returns
  `NotSupported`; VZ memory size is fixed at VM construction here.
- **`GET /vm/config`** — routed but returns `NotSupported`; hephaestus
  does not expose Firecracker's runtime VM config toggles.
- **`PATCH /vm` with anything other than Paused/Resumed** — upstream
  has `RestoreVm`/`CreateSnapshot`/etc.; hephaestus uses the
  dedicated `/snapshot/*` endpoints instead.

## Regression harness

`compat/firectl-harness/` is a ~250-line Go binary that drives our
server through `firecracker-go-sdk`'s swagger-generated client —
same marshaling + strict deserializer that real `firectl` and Kata
use. Run after every upstream rebase:

```bash
just fc-compat-config          # CI-safe config-only run with dummy artifacts
just fc-compat-sandbox-config  # CI-safe config-only run under a restrictive sandbox
just fc-compat 0               # alias for the config-only path
just fc-compat                 # cold boot with real kernel/rootfs artifacts
just fc-compat-sandbox         # cold boot under a restrictive sandbox
just fc-compat-vsock-e2e       # real-VM /vsock bridge + guest MMDS smoke
just fc-compat-sandbox-vsock-e2e # same, under a restrictive sandbox
just fc-compat-pool            # warm-pool restore (agent flavor)
just fc-compat-sandbox-pool    # same, under a restrictive sandbox
just fc-compat-pool-stock      # warm-pool restore (stock-init flavor)
just fc-compat-sandbox-pool-stock # same, under a restrictive sandbox
just fc-compat-snapshot        # save/stop/fresh-process/load round-trip
just fc-compat-sandbox-snapshot # same, under a restrictive sandbox
```

`fc-compat-config` runs in GitHub Actions on every PR and catches
wire-shape drift without booting a VM. The booting variants require real
apple/container kernel + rootfs artifacts and remain local/e2e smokes.
`fc-compat-vsock-e2e` is headless but boots a real VM: it configures MMDS,
configures `PUT /vsock`, reaches the guest agent through Firecracker's UDS
`CONNECT 1234` bridge, asks the agent to fetch MMDS from guest vsock port
`16992`, validates the guest-side `169.254.169.254:80` MMDS shim, and validates
an arbitrary guest-port echo exchange over the same UDS bridge.
