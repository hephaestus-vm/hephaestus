# hephaestus — roadmap toward Firecracker feature-completeness

Companion to [COMPAT.md](COMPAT.md) (per-endpoint status) and
[hephaestus-progress.md](hephaestus-progress.md) (session history). This
file tracks *forward* work: the buildable gaps between today and a
drop-in Firecracker replacement, in priority order.

## What "feature complete" means here

"Complete" is bounded by what Apple's Virtualization.framework can
express. A set of Firecracker features are **permanently out of scope**
because VZ has no equivalent — they can only ever be documented
`NotSupported`, never turned green:

- Diff snapshots + dirty-page tracking (no VZ dirty-page API)
- UFFD memory backend (Linux userfaultfd)
- CPU templates / `PUT /cpu-config` (Apple Silicon feature control)
- Post-boot drive/NIC hot-swap (VZ attachments are construction-time)
- pmem device, memory hotplug
- Cross-tool snapshot interop (hypervisor-specific blob formats)

So the target is: **close every gap VZ can actually express.** The
milestones below are that list, highest leverage first.

## Status at a glance

| Milestone | State |
| :-- | :-- |
| M1a — NAT networking on the HTTP path | ✅ Done |
| M2 — device breadth (data drives, entropy, SendCtrlAltDel, balloon) | ✅ Done |
| M3 — metrics fidelity & operability | ✅ Done (rate limiters deferred) |
| M1b — MMDS over the guest NIC (vmnet, restricted entitlement) | ⬜ Not started |
| M4 — multi-tenant / jailer productionization | ▶ In progress |

---

## Milestone 1 — Guest networking (the headline gap)

**Why first:** a VM booted over the HTTP API currently has **no NIC at
all** (`buildLongRunningConfig` wires no network device). Kata / firectl
/ fly-style orchestrators all assume guest networking, so this is the
single biggest blocker to real drop-in use. The CLI container path
already proves VZ NAT works with only the base
`com.apple.security.virtualization` entitlement, so the base tier needs
no new entitlements.

### M1a — NAT networking on the HTTP path (base entitlement) — ✅ Done

Delivered: `PUT /network-interfaces/{id}` now attaches a
`VZNATNetworkDeviceAttachment` NIC (honoring `guest_mac`) via
`enable_networking`/`mac` threaded through `hb_vz_long_new`/`_restore`;
the once-dead `iface` field drives it. Verified by `just
fc-compat-net-e2e` (guest sees a non-loopback netdev). The original plan
is kept below as design history.

Give HTTP-API VMs outbound IPv4 + a stable NAT address, honoring
`PUT /network-interfaces/{id}` instead of accept-and-ignore.

- **Swift** (`DirectVZ.swift`, `buildLongRunningConfig`): add a
  `VZVirtioNetworkDeviceConfiguration` with a
  `VZNATNetworkDeviceAttachment` when networking is requested; set
  `VZMACAddress(string:)` when the client supplied `guest_mac`.
- **FFI** (`hb_vz_long_new`, `hb_vz_long_restore`): add
  `enable_networking: bool` + `mac: *const c_char` params, following the
  exact `read_only` threading pattern just landed (extern decl + Swift
  `@_cdecl` positions must stay in lockstep).
- **Rust bridge** (`VzSpec`): add `networking: bool`, `mac: Option<String>`
  builder fields; pass through in `VzVm::new` / `vz_long_restore`.
- **Backend** (`backend.rs`): stop dropping the already-stored (today
  dead) `iface` field — derive `networking = iface.is_some()` and
  `mac = iface.guest_mac`, thread into `VzSpec`. Reuse the existing
  `allocate_ip_octet` helper if we want deterministic per-VM addresses.
- **Semantics note for COMPAT.md:** VZ NAT hands the guest a DHCP lease
  in `192.168.64.0/24` (gateway `.1`). We honor "attach a NIC" and MAC,
  but not host tap name or rate limiters (VZ doesn't expose them). A
  client that needs a *specific* guest IP sets it via `boot_args` /
  cloud-init, not the tap config.

**Verify:** unit test (iface → spec plumbing), then a networking e2e —
reuse the vsock command channel (`hephaestus-agent` runs arbitrary
`/bin/sh` over port 1234) to run `ip -4 addr` + `ping -c1 192.168.64.1`
+ an outbound DNS/HTTP reach and assert exit 0. Model it on
`scripts/fc-compat-vsock-e2e.sh`.

### M1b — MMDS over the guest NIC (restricted entitlement, dependent)

> **Environment prerequisite:** M1b needs the restricted
> `com.apple.vm.networking` entitlement. See [DEV_ENV.md](DEV_ENV.md) for the
> feasibility probe (`just probe-vmnet`) and the provisioning-profile setup —
> on the current machine the probe reports `Killed: 9` (blocked until a
> profile is installed).

Make stock images reach `169.254.169.254` **without** our agent shim.
This is *not* unblocked by NAT: VZ's NAT is a black box we can't inject
an MMDS responder into (that's exactly why the agent shim exists). It
requires bridged `VZVmnetNetworkDeviceAttachment` + the restricted
`com.apple.vm.networking` entitlement so the host can run the existing
`host_mmds.rs` listener on the guest's L2 and the guest can route to it.

- Gate behind the entitlement; keep the agent-shim path as the
  base-entitlement default.
- Flip `--host-mmds` from scaffold to real once vmnet is wired.
- Ship the codesigning/entitlement recipe (`link-and-sign.sh` already
  exists as the hook).

**Verify:** boot a **stock** (no hephaestus-agent) image and
`curl 169.254.169.254/latest/meta-data` from inside the guest.

---

## Milestone 2 — Device breadth — ✅ Done

All four items shipped, each with a real-VM smoke and a
firectl-harness/COMPAT update:

- **Secondary / data drives** — done. Backend tracks a root + ordered
  secondary drive list keyed by `drive_id`; the FFI carries a
  `(paths, readonly)` array; the guest sees `/dev/vdb…`. Pool fast-path
  skipped when secondaries are configured.
- **`PUT /entropy` → supported** — done (device was already attached;
  endpoint now confirms).
- **`SendCtrlAltDel` → graceful stop** — done via
  `hb_vz_long_request_stop` → VZ `requestStop()`.
- **Memory balloon** — done. VZ traditional balloon always attached;
  `PUT`/`GET`/`PATCH /balloon` map `amount_mib` onto
  `targetVirtualMachineMemorySize`; validated at boot; save/restore
  verified compatible. `/balloon/statistics` + hinting stay
  `NotSupported`.

The original plan is kept below as design history.

### Original plan (design history)

- **Secondary / data drives** — VZ supports multiple
  `VZVirtioBlockDeviceConfiguration` attachments. Replace the single
  `root_drive: Option<PathBuf>` with an ordered drive list, attach all,
  drop the `is_root_device: false → NotSupported` rejection. Honor
  per-drive `is_read_only` (already wired for root). *Kata attaches a
  data drive; this is the most-requested Tier-2 item.*
- **`PUT /entropy` → supported** — nearly free: `buildLongRunningConfig`
  already attaches `VZVirtioEntropyDeviceConfiguration` unconditionally,
  so the device exists; just stop returning `NotSupported` and confirm
  the wire shape.
- **`SendCtrlAltDel` → graceful stop** — map the action to VZ
  `requestStop()` (ACPI shutdown request) instead of `NotSupported`.
  Small FFI addition (`hb_vz_long_request_stop`).
- **Memory balloon** — VZ has
  `VZVirtioTraditionalMemoryBalloonDeviceConfiguration`. Wire
  `PUT /balloon` (+ `PATCH` target size) to
  `.targetVirtualMachineMemorySize`. Moderate; `/balloon/statistics`
  and `/balloon/hinting/*` stay `NotSupported` (no VZ equivalent).

**Verify:** extend the firectl-harness config coverage per endpoint;
data-drive + balloon get real-VM smokes (guest sees `/dev/vdb`; balloon
target shrinks resident memory).

---

## Milestone 3 — Fidelity & operability — ✅ Done

- **Real metrics counters** — done. `ApiCounters` classifies each
  request by `(method, path)` so every `*_api_requests.*` field is real;
  device/hypervisor counters with no VZ equivalent stay zero. The metrics
  sink handle is held open (no per-request reopen). Locked in by the
  firectl-harness (asserts counters are non-zero).
- **Review nits** — done: guest-agent handshake read timeout (a stalled
  probe no longer wedges the serial accept loop), Swift `errno`-before-
  `close`, `ExitFlagBox` lock-guarded test-and-set, and the
  snapshot-recipe EXIT-trap unbound-var leak.
- **Rate limiters** — *deferred, documented-noop.* `drive` / `net`
  `rate_limiter` are accepted and ignored; VZ exposes no token-bucket
  knob, so this is low value / high effort. Revisit only if a data-path
  shaper becomes worthwhile.

---

## Milestone 4 — Multi-tenant / jailer productionization

Separate track: no new API surface, but it's what lets us drop the
"don't run untrusted guests" caveat. Builds on the sandbox hardening
just landed (`--id` validation, private work-root, least-privilege pool
grant). **Done:** process-group ownership + signal forwarding, and
`--rlimit-*` resource caps on the daemon (`just jailer-rlimit-check`).
**Remaining:** uid/gid drop (needs a service user + `sudo` to verify) and a
launchd supervisor. See [DEV_ENV.md](DEV_ENV.md) for the setup.

- Finish `JAILER_MMDS_PLAN.md`: uid/gid drop, per-VM resource limits,
  launchd/process-group ownership so a killed jailer reaps its daemon
  (the orphaned-daemon leak we hit during e2e).
- Signal forwarding + `setpgid` so `hephaestus-jailer` teardown is
  complete.
- Revisit the shared-temp-root story for the multi-tenant threat model.

---

## Suggested execution order

1. ~~**M1a** (NAT networking)~~ — ✅ done.
2. ~~**M2** (entropy, SendCtrlAltDel, data drives, balloon)~~ — ✅ done.
3. ~~**M3** (metrics fidelity + review nits)~~ — ✅ done (rate limiters deferred).
4. **M4** (jailer) — process ownership + isolation *(current)*.
5. **M1b** (vmnet MMDS) — the restricted-entitlement track.

Every milestone lands with: unit tests, a COMPAT.md status update, a
firectl-harness case, and — for anything that boots — a real-VM e2e
smoke following the `fc-compat-*` pattern.
