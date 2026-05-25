# Jailer + link-local MMDS plan

This captures the direction chosen for the next compatibility/hardening pass:
prioritize Firecracker-heavy users, assume a multi-tenant local-service threat
model, allow a future Developer ID entitlement path, and split tests between
CI-safe control-plane coverage and local real-VM e2e.

## Link-local MMDS

### Goal

Firecracker guests commonly fetch metadata from:

```text
http://169.254.169.254/
```

hephaestus already stores Firecracker MMDS JSON and exposes it to guests over
VZ vsock port `16992`. The compatibility gap is the in-guest link-local IP
transport.

### Compatibility ranking

1. **Transparent host-network MMDS** is most compatible with arbitrary existing
   Firecracker images: no guest changes, same URL, same client behavior.
2. **Guest-side shim** is the practical first step: it makes the same URL work
   for images we control or images that opt into a small helper, by forwarding
   link-local HTTP to the existing vsock MMDS service.

We are shipping the guest-side path first and keeping the host-network path as
the later maximum-compatibility target.

### Guest-side shim shape

The controlled-image shim should:

1. Configure `169.254.169.254/32` on loopback inside the guest.
2. Listen on `169.254.169.254:80`.
3. Forward HTTP requests to host vsock port `16992`.
4. Return the host's MMDS HTTP response unchanged where practical.

The current real-VM e2e path exercises this in `hephaestus-agent` with an
internal command used only by tests. That proves the transport before we turn it
into a user-facing helper/package/initramfs feature.

### E2E plan

- **CI-safe:** `fc-compat-config` validates `PUT /mmds/config` accepts
  `169.254.169.254` and rejects non-link-local IPv4 addresses.
- **Local real-VM:** `fc-compat-vsock-e2e` boots a VM and now validates:
  - host UDS `CONNECT 1234` to the guest agent,
  - guest vsock fetch from host MMDS port `16992`,
  - guest link-local fetch through the shim at `169.254.169.254:80`,
  - generic non-MMDS vsock echo traffic.

### Host-network path, later

For arbitrary guest images, investigate a transparent host-network path after
the Developer ID/signing story is available. Likely areas:

- `VZVmnetNetworkDeviceAttachment` instead of built-in NAT,
- a privileged helper or Network Extension if packet interception requires it,
- `pf`/routing integration under a signed/entitled install,
- a release-gated real-Mac e2e job.

## Full jailer direction

### Goal

Support a **multi-tenant local service** model: mutually-untrusted tenants can
run VMs on one Mac with damage constrained if a guest or VM-adjacent process
finds an escape.

### Near-term hook

`hephaestus-firecracker --sandbox-profile <file>` now enters a caller-supplied
macOS sandbox profile before serving requests. This is only a primitive; it is
not a jailer by itself.

### Target architecture

A full jailer should own process launch, profile generation, and lifecycle:

1. Supervisor creates a per-VM working directory.
2. Supervisor materializes/canonicalizes allowed paths:
   - API socket,
   - kernel/rootfs/initrd,
   - log/metrics,
   - vsock UDS,
   - snapshot files,
   - pool slot/rootfs clone if used.
3. Supervisor generates a deny-by-default sandbox profile with only those paths
   and required system/VZ framework access.
4. Supervisor launches one `hephaestus-firecracker` process per VM/tenant under
   that profile.
5. Optional later step: use launchd jobs or a dedicated signed helper for
   stronger process-boundary ownership.

### Test plan

- **CI-safe permissive:** keep permissive `--sandbox-profile` startup in
  `fc-compat-config` so the hook cannot regress on GitHub CI.
- **CI-safe restrictive config-only:** `fc-compat-sandbox-config` generates a
  deny-by-default profile allowing only the temp API socket/work dir plus dummy
  kernel/rootfs inputs, proves an unrelated file is denied with the daemon's
  hidden deny probe, then runs the Go SDK compat harness with `-skip-boot`.
- **Local real-VM restrictive:** sandbox coverage now spans the full local
  Firecracker-compat smoke family:
  - `fc-compat-sandbox` — cold boot + pause/resume under a generated profile.
  - `fc-compat-sandbox-vsock-e2e` — vsock/MMDS/link-local MMDS under a generated profile.
  - `fc-compat-sandbox-snapshot` — save in one sandboxed process, load in another.
  - `fc-compat-sandbox-pool` — agent-flavor warm-pool restore.
  - `fc-compat-sandbox-pool-stock` — stock-init warm-pool restore.
- **Release gate:** when Developer ID entitlements are in play, run the
  restrictive real-VM suite on a dedicated Mac runner or manual release
  checklist.

## Non-goals for this pass

- Transparent host-network MMDS for arbitrary images.
- A complete launchd/supervisor jailer.
- Claiming untrusted guests are supported before restrictive real-VM e2e passes.
