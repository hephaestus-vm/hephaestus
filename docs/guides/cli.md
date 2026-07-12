# CLI guide

The `hephaestus` binary provides process-oriented VM execution, direct
Virtualization.framework workflows, snapshots, and warm pools.

Examples assume the binary is on `PATH`. A source build is available at
`build/cargo_target/debug/hephaestus` after `cargo build --workspace`.

## Discover the interface

```console
$ hephaestus --help
$ hephaestus --version
$ hephaestus run --help
$ hephaestus pool --help
$ hephaestus pool init --help
```

Every command and nested command supports `-h` and `--help`.

## Run a guest process

`hephaestus run` uses Apple's Containerization library. Supply the arm64 Linux
kernel, Containerization init filesystem, writable root filesystem, and guest
process arguments:

```console
$ hephaestus run \
    --id example \
    --kernel "$KERNEL" \
    --initfs "$INITFS" \
    --rootfs "$ROOTFS" \
    --cpus 2 \
    --memory-mib 512 \
    -- /bin/uname -a
```

Add `--network` to attach VZ NAT and `--tty` for an interactive process:

```console
$ hephaestus run \
    --id shell \
    --kernel "$KERNEL" \
    --initfs "$INITFS" \
    --rootfs "$ROOTFS" \
    --network \
    --tty \
    -- /bin/sh
```

With networking enabled, VZ uses `192.168.64.0/24`. `--ip` accepts either the
last octet or a complete address in that subnet.

Everything after `--` belongs to the guest process. For example, guest help is
not interpreted as Hephaestus help:

```console
$ hephaestus run ... -- /bin/tool --help
```

## Build a root filesystem

The `rootfs` command converts a plain, gzip, or zstd-compressed tar archive into
an ext4 image:

```console
$ hephaestus rootfs \
    --from-tar rootfs.tar.gz \
    --output rootfs.ext4 \
    --size-mib 512
```

Compression is detected automatically.

## Direct-VZ execution

`vz-exec` boots the bundled guest agent and runs one shell command:

```console
$ hephaestus vz-exec \
    --kernel "$KERNEL" \
    --rootfs "$ROOTFS" \
    --initramfs "$AGENT_INITRAMFS" \
    --cmd 'uname -a; echo "$HOSTNAME"'
```

Use `--stdin` to forward host standard input to the guest command. `vz-sh`
opens a direct-VZ shell against a stock rootfs:

```console
$ hephaestus vz-sh \
    --kernel "$KERNEL" \
    --rootfs "$ROOTFS"
```

## Agent-ready snapshots

Save a VM while the agent is waiting for work, then restore it to execute a
command:

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

Kernel, rootfs, initramfs, CPU, and memory configuration must match between save
and restore. See [Snapshots](snapshots.md).

## Warm pools

The `pool` command turns agent-ready or stock-init snapshots into reusable slot
sets:

```console
$ hephaestus pool init --help
$ hephaestus pool run --help
```

Follow the [Warm pools guide](warm-pools.md) for complete binary workflows.

## Repository shortcuts

When working from the source tree, scripts and `just` recipes discover and clone
`apple/container` artifacts before invoking the same binary commands:

```console
$ just hello
$ just shell
$ just sh
$ just vz-exec 'uname -a'
```

Use these for development convenience. Automation and user-facing examples
should invoke `hephaestus` directly.

## Choosing a path

| Need | Binary command |
| :-- | :-- |
| Run one process with terminal/network integration | `hephaestus run` |
| Execute through the direct VZ bridge | `hephaestus vz-exec` |
| Open a stock guest shell | `hephaestus vz-sh` |
| Save or restore agent-ready state | `hephaestus vz-warm` |
| Save or restore stock guest state | `hephaestus vz-snapshot` |
| Manage reusable VM slots | `hephaestus pool` |
| Serve Firecracker clients | `hephaestus-firecracker` |
