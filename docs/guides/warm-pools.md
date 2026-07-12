# Warm pools

The `hephaestus` binary manages disk-persistent pools of pre-snapshotted VMs.
A pool restores a ready machine instead of cold-booting it. Slots use APFS
copy-on-write rootfs clones and are claimed by one caller at a time.

Examples assume `hephaestus` and `hephaestus-firecracker` are on `PATH`. For a
source build, use `build/cargo_target/debug/<binary>` instead.

## Prepare artifacts

Set paths to an arm64 Linux kernel, writable ext4 root filesystem, and the
Hephaestus agent initramfs:

```console
$ export KERNEL=/absolute/path/to/vmlinux
$ export ROOTFS=/absolute/path/to/rootfs.ext4
$ export INITRAMFS=/absolute/path/to/agent.cpio.gz
$ export POOL=/tmp/hephaestus-pool
```

The kernel and rootfs must remain available at the same canonical paths when the
pool is used.

## Create an agent pool

An agent-flavor pool accepts commands through the `hephaestus pool run`
interface:

```console
$ hephaestus pool init \
    --dir "$POOL" \
    --kernel "$KERNEL" \
    --rootfs "$ROOTFS" \
    --initramfs "$INITRAMFS" \
    --size 4 \
    --cpus 2 \
    --memory-mib 512
```

Inspect and use it:

```console
$ hephaestus pool stats --dir "$POOL"
$ hephaestus pool run \
    --dir "$POOL" \
    --cmd 'uname -a; echo ready'
```

`pool run` returns the guest command's exit status. It never waits for a free
slot; when every slot is busy it exits with status 75, and the caller owns retry
or queueing. Set `HEPHAESTUS_POOL_LOG=<path>` to capture guest serial output.

Remove the pool when it is no longer needed:

```console
$ hephaestus pool destroy --dir "$POOL"
```

## Create a stock-init pool

A stock-init snapshot boots the rootfs's normal init process without the agent,
initramfs, or command channel:

```console
$ hephaestus pool init \
    --dir "$POOL" \
    --kernel "$KERNEL" \
    --rootfs "$ROOTFS" \
    --size 4 \
    --cpus 2 \
    --memory-mib 512 \
    --stock-init \
    --settle-seconds 3
```

`hephaestus pool run` intentionally rejects stock-init pools because they have
no command channel. This flavor is intended for `hephaestus-firecracker`, where
a restored guest should resemble a cold-booted Firecracker image.

## Attach a pool to the Firecracker daemon

```console
$ hephaestus-firecracker \
    --api-sock /tmp/hephaestus.sock \
    --id example \
    --pool-dir "$POOL"
```

On `InstanceStart`, the daemon looks for a slot whose match key contains the
canonical kernel and rootfs paths, vCPU count, and memory size. A miss falls
back to cold boot. Secondary drives also force a cold boot. Boot arguments are
not part of the key because a restored VM resumes with the command line captured
in the saved state.

The Firecracker client must configure the same kernel, rootfs, CPU count, and
memory size used by `pool init`.

## Repository shortcuts

When developing from this repository, these recipes discover cached
`apple/container` artifacts and wrap the binary commands above:

```console
$ just pool-init 4
$ just pool-stats
$ just pool-run 'uname -a'
$ just pool-destroy
```

The recipes are conveniences, not a separate public interface.

## Operational constraints

- Pool metadata, machine identifiers, and save files must remain together.
- Do not bypass pool ownership or share a writable slot rootfs.
- Rebuild pools after incompatible guest, device, or host-framework changes.
- Pool state is specific to Virtualization.framework.
- Pool-restored VMs cannot be converted into ordinary API snapshots.

## Command help and validation

Use nested CLI help for the full option reference:

```console
$ hephaestus pool --help
$ hephaestus pool init --help
$ hephaestus pool run --help
```

Repository maintainers can validate both pool flavors with
`just fc-compat-pool` and `just fc-compat-pool-stock`. See
[Performance](../performance.md) for measured restore times.
