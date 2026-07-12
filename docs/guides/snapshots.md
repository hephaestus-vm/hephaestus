# Snapshots

Hephaestus uses Virtualization.framework's `saveMachineStateTo:` and
`restoreMachineStateFrom:` APIs. A snapshot can be moved between compatible
Hephaestus processes, but not between Hephaestus and Firecracker.

## Firecracker API workflow

Pause a running VM before creating a snapshot:

```console
$ curl --unix-socket "$sock" -X PATCH http://localhost/vm \
    -H 'Content-Type: application/json' \
    -d '{"state":"Paused"}'

$ curl --unix-socket "$sock" -X PUT http://localhost/snapshot/create \
    -H 'Content-Type: application/json' \
    -d '{"snapshot_type":"Full","snapshot_path":"/tmp/vm.save","mem_file_path":"/tmp/vm.mem"}'
```

Virtualization.framework produces one combined state blob. Hephaestus writes it to `snapshot_path` and creates an empty compatibility stub at `mem_file_path`. Diff snapshots and dirty-page tracking are unsupported.

Load a snapshot in a fresh `hephaestus-firecracker` process. Configure the same kernel, rootfs, vCPU count, memory size, initramfs, and construction-time
devices first, then call `/snapshot/load`. Set `resume_vm` to `false` to remain paused after loading.

## CLI workflows

Use `hephaestus vz-warm` for an agent snapshot that waits for a command after
restore:

```console
$ hephaestus vz-warm save \
    --kernel "$KERNEL" \
    --rootfs "$ROOTFS" \
    --initramfs "$AGENT_INITRAMFS" \
    --save /tmp/agent.save

$ hephaestus vz-warm run \
    --kernel "$KERNEL" \
    --rootfs "$ROOTFS" \
    --initramfs "$AGENT_INITRAMFS" \
    --save /tmp/agent.save \
    --cmd 'uname -a'
```

Use `hephaestus vz-snapshot save|restore` for stock guest state:

```console
$ hephaestus vz-snapshot save \
    --kernel "$KERNEL" --rootfs "$ROOTFS" \
    --log /tmp/guest.log --save /tmp/stock.save

$ hephaestus vz-snapshot restore \
    --kernel "$KERNEL" --rootfs "$ROOTFS" \
    --log /tmp/restored.log --save /tmp/stock.save
```

Run `hephaestus vz-warm --help` or `hephaestus vz-snapshot --help` to discover
nested commands and their full options. Source-tree users can use
`just vz-warm-save` and `just vz-warm-run` as artifact-discovery shortcuts.

## Compatibility constraints

A saved VM is coupled to:

- the kernel and rootfs paths and contents;
- the VM's CPU, memory, devices, initramfs, and boot configuration;
- its generated VZ machine identifier;
- the host framework's snapshot format.

A configuration mismatch is rejected by Virtualization.framework. Regenerate
snapshots after changing guest devices, agent serial layout, or incompatible
host software.

Pool-restored VMs cannot be saved through `/snapshot/create` because their
agent/stock pool configuration cannot be reproduced by the ordinary snapshot
loader. Cold-boot a fresh VM before taking an API snapshot.

## Validation

Run the complete save, process restart, and load smoke test with:

```console
$ just fc-compat-snapshot
```

The sandboxed equivalent is `just fc-compat-sandbox-snapshot`. Both require
local guest artifacts and boot real VMs.
