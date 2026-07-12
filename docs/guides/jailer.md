# Jailer

`hephaestus-jailer` is an experimental per-VM supervisor. It prepares a private working directory, generates a deny-by-default macOS sandbox profile, and starts one `hephaestus-firecracker` process under that profile.

> [!WARNING]
> This is hardening work in progress, not support for hostile or mutually
> untrusted guests. Read [SECURITY.md](../../SECURITY.md).

## What it does

Before launching the daemon, the jailer:

1. Validates the VM identifier as one safe path component.
2. Creates a private `<work-dir>/<id>/` directory.
3. Canonicalizes the kernel, rootfs, initramfs, pool, and daemon paths.
4. Generates a sandbox profile allowing required framework access, the VM's
   inputs, and its work directory.
5. Places the API socket and generated profile under the work directory.
6. Applies optional file-descriptor, process, and file-size limits.
7. Starts the daemon in a process group and forwards termination signals.

## Example

Examples assume `hephaestus-jailer` and `hephaestus-firecracker` are on `PATH`.
For source builds, use their paths under `build/cargo_target/debug/`.

```console
$ hephaestus-jailer \
    --id example \
    --kernel /absolute/path/to/vmlinux \
    --rootfs /absolute/path/to/rootfs.ext4 \
    --rlimit-nofile 256 \
    --rlimit-nproc 128 \
    --rlimit-fsize 1073741824
```

Use `--initramfs` and `--pool-dir` when those files are part of the VM. The
jailer defaults to finding `hephaestus-firecracker` on `PATH`; pass
`--firecracker-binary` to select a specific build.

Validate resource-limit plumbing without booting a VM:

```console
$ just jailer-rlimit-check
```

Restrictive sandbox tests cover config-only, cold boot, vsock/MMDS, snapshots,
and both pool flavors. See [Testing](../development/testing.md).

## Security boundary

The current jailer does not provide:

- uid/gid isolation;
- chroot or mount namespaces;
- Linux cgroups or seccomp;
- a launchd service that owns and restarts a fleet of VMs;
- a completed signed distribution and entitlement model;
- a claim that escaped guest code is contained from other local tenants.

Virtualization.framework remains the primary guest/host boundary. The macOS
sandbox narrows the daemon's filesystem and process access, but profile
generation and lifecycle controls are not a complete multi-tenant design.

## Direct sandbox hook

`hephaestus-firecracker --sandbox-profile <file>` applies a caller-supplied
profile before binding the API socket. This low-level hook exists for tests and
custom supervisors. The caller must allow every kernel, rootfs, socket, log,
metrics, snapshot, and pool path that the API may use.
