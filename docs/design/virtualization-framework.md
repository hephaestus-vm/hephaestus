# Virtualization.framework mapping

Hephaestus preserves Firecracker's control-plane model where Apple's
Virtualization.framework can express equivalent behavior. It documents or
rejects operations where the hypervisors differ.

## Resource mapping

| Firecracker concept | Hephaestus implementation |
| :-- | :-- |
| VMM process | One `hephaestus-firecracker` process |
| Machine configuration | `VZVirtualMachineConfiguration` CPU and memory |
| Linux boot source | `VZLinuxBootLoader` |
| Block drive | `VZVirtioBlockDeviceConfiguration` |
| Network interface | `VZVirtioNetworkDeviceConfiguration` with VZ NAT or shared vmnet |
| Entropy device | `VZVirtioEntropyDeviceConfiguration` |
| Memory balloon | `VZVirtioTraditionalMemoryBalloonDeviceConfiguration` |
| Vsock | `VZVirtioSocketDeviceConfiguration` and host UDS bridge |
| Pause/resume | `VZVirtualMachine.pause` and `resume` |
| Full snapshot | `saveMachineStateTo:` and `restoreMachineStateFrom:` |
| Graceful stop | VZ `requestStop()` |

## Semantic differences

VZ devices are generally fixed when constructing a VM, so live drive and NIC
replacement are unavailable. NAT is managed by VZ rather than a caller-created
tap device. The opt-in vmnet path adds a user-space packet interface for
transparent MMDS. VZ's save file is one hypervisor-specific blob rather than
Firecracker's state and memory files.

Several Firecracker features have no public VZ equivalent:

- CPU templates and CPU feature control;
- dirty-page tracking and diff snapshots;
- userfaultfd memory backends;
- pmem devices;
- memory hotplug;
- device token-bucket rate limiters;
- cross-hypervisor snapshot loading.

Unsupported behavior should return a structured error rather than silently
claim equivalence. Accepted-but-ignored fields are limited to cases needed for
real client interoperability and are listed in
[Firecracker compatibility](../firecracker-compatibility.md).

## Isolation

Virtualization.framework is the primary guest/host isolation boundary. The base
build is ad-hoc signed with `com.apple.security.virtualization`. Some network
attachments require the restricted `com.apple.vm.networking` entitlement and a
provisioning profile; these are not part of the ordinary build. See
[Privileged features](../development/privileged-features.md).
