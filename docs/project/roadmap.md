# Roadmap

Hephaestus is working toward the largest practical Firecracker-compatible
surface that Apple's Virtualization.framework can support. This document is
forward-looking; completed implementation history is kept in the
[changelog](../../CHANGELOG.md) and Git history.

## Current state

The core HTTP lifecycle, CLI execution, NAT networking, data drives, entropy,
ballooning, metrics, full snapshots, warm pools, vsock bridging, MMDS control
plane, and experimental sandbox supervisor work end to end.

The project remains alpha because the deployment/security model and public
stability contract are incomplete.

## Active priorities

### 1. Jailer productionization

Goal: replace the trusted-workload-only warning with a defensible local service
model.

Remaining work includes:

- privilege drop to a dedicated uid/gid where deployment permits it;
- launchd ownership and restart behavior;
- complete per-VM path and lifecycle ownership;
- release-gated restrictive real-Mac tests;
- a reviewed threat model for shared host resources;
- signed distribution and entitlement guidance.

Completion requires more than a passing sandbox smoke. The security policy must
explicitly support the intended tenant model.

### 2. Transparent MMDS for stock images

Goal: make `169.254.169.254` reachable from an arbitrary compatible guest image
without the bundled agent shim.

The likely implementation requires vmnet/bridged networking and the restricted
`com.apple.vm.networking` entitlement. Work remains gated by an authorized
signing and test environment. NAT plus the agent/vsock shim remains the default
base-entitlement path.

### 3. Public API and release stability

Before v1.0:

- define the supported Firecracker version range;
- stabilize CLI and daemon flags;
- document snapshot and pool upgrade compatibility;
- establish a supported macOS/Xcode window;
- automate release smoke tests and documentation checks.

## Permanently out of scope without new Apple APIs

- Firecracker/KVM snapshot interoperability
- Diff snapshots and dirty-page tracking
- Linux userfaultfd memory backends
- Firecracker CPU templates and CPU feature control
- Post-boot drive or NIC replacement
- Pmem devices
- Memory hotplug

These operations should remain explicit `NotSupported` behavior rather than
compatibility promises.

## Delivery standard

Every compatibility change ships with:

1. unit coverage for state and validation;
2. a Go SDK harness case when the wire surface changes;
3. a real-VM smoke when execution behavior changes;
4. an update to the canonical compatibility document;
5. security and performance review where the affected boundary requires it.
