# Guest images

Hephaestus boots Linux guests from host files. The exact inputs depend on the VM path.

## Inputs

- **Kernel:** an uncompressed arm64 Linux kernel accepted by
  `VZLinuxBootLoader`.
- **Root filesystem:** an ext4 filesystem image containing the guest userspace.
- **Initramfs/initfs:** optional for stock boot; required by paths using the
  bundled `hephaestus-agent`.

All files must be readable by the Hephaestus process. Writable drives should be cloned before reuse because a running guest can mutate them.

## Using apple/container artifacts

For local development, the supported recipes discover artifacts from the
`apple/container` cache:

```console
$ container system start
$ container run --rm docker.io/library/alpine:3.20 echo ready
$ just artifacts
```

The recipes identify the cached kernel and distinguish the smaller initfs from
the larger rootfs. Do not depend on the cache paths in external automation;
they are an implementation detail of `apple/container`.

## Building an ext4 root filesystem

The `hephaestus` binary accepts plain, gzip, or zstd-compressed tar archives
and writes an ext4 image:

```console
$ hephaestus rootfs \
    --from-tar rootfs.tar.gz \
    --output rootfs.ext4 \
    --size-mib 512
```

Run `hephaestus rootfs --help` for the complete option reference. Source-tree
users can use `just rootfs-build rootfs.tar rootfs.ext4 512` as a wrapper.

## Agent-based images

`just build-agent` cross-compiles `guest/hephaestus-agent` for
`aarch64-unknown-linux-musl` and creates `build/agent.cpio.gz`. The direct-VZ
execution and agent warm-pool paths boot this agent as PID 1. It mounts and
enters the rootfs, accepts commands over vsock, and powers off after the command
exits.

The stock-init path instead boots the root filesystem's own init process. Use it when the guest should behave like a normal Firecracker image and no command
channel is required.

## Kernel and filesystem compatibility

Hephaestus currently targets arm64 Linux guests. A custom kernel needs the
virtio drivers for the devices you configure, such as block, network, balloon,
entropy, console, and vsock. Network addressing remains the guest's
responsibility; see [Networking and MMDS](networking.md).
