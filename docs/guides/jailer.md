# Jailer

`hephaestus-jailer` is a per-VM supervisor. It prepares a private working directory, generates a deny-by-default macOS sandbox profile, and starts one `hephaestus-firecracker` process under that profile.

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
5. Places the API socket under the per-VM work directory and keeps the
   generated profile in the root-owned parent.
6. Applies optional file-descriptor, process, and file-size limits.
7. Optionally drops privileges to an unprivileged uid/gid (requires root),
   handing the per-VM work dir to the target user while keeping the generated
   sandbox profile root-owned outside that writable directory.
8. Starts the daemon in a process group and forwards termination signals.

## Privilege drop

The jailer can drop root privileges before exec'ing the daemon:

- `--uid <n> --gid <n>` — drop to explicit numeric uid/gid.
- `--uid <n>` — drop uid only (gid unchanged).
- `--gid <n>` — drop gid only (uid unchanged).
- `--user <name>` — look up the user's uid and gid from the passwd database.

All require the jailer to be started as root. The drop order is
setgroups → setgid → setuid: supplementary groups are replaced with just the
target gid while still root (otherwise the daemon would keep root's
wheel/admin memberships), and gid is dropped before uid (once uid is dropped,
setgid would fail). The jailer verifies the drop is irreversible and chowns
the per-VM work dir to the target uid/gid so the daemon can create the API
socket afterwards. The generated profile remains root-owned outside the
writable work dir, preventing a compromised daemon from replacing it before a
restart.

```console
$ sudo hephaestus-jailer \
    --id example \
    --kernel /absolute/path/to/vmlinux \
    --rootfs /absolute/path/to/rootfs.ext4 \
    --user nobody
```

## launchd supervision

The jailer can generate a launchd plist instead of running directly. Run the
generator as root so the captured work root has the same ownership and default
path as the eventual LaunchDaemon:

```console
$ sudo hephaestus-jailer \
    --id example \
    --kernel /absolute/path/to/vmlinux \
    --rootfs /absolute/path/to/rootfs.ext4 \
    --user nobody \
    --generate-launchd-plist
```

This writes a plist to stdout. The plist re-runs the full jailer invocation
(with the resolved daemon path pinned via `--firecracker-binary`) and uses
`KeepAlive` so launchd restarts the VM whenever the supervised daemon exits.

The plist is intended for `/Library/LaunchDaemons`: launchd must start the
jailer as root so it can generate the sandbox profile and drop privileges —
a per-user LaunchAgent cannot do this. Use `--launchd-plist-path <file>`
(which implies `--generate-launchd-plist`) to write it in place, then load
it:

```console
$ sudo hephaestus-jailer \
    --id example \
    --kernel /absolute/path/to/vmlinux \
    --rootfs /absolute/path/to/rootfs.ext4 \
    --user nobody \
    --launchd-plist-path /Library/LaunchDaemons/com.hephaestus.vm.example.plist
$ sudo launchctl bootstrap system /Library/LaunchDaemons/com.hephaestus.vm.example.plist
```

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

Validate privilege-drop plumbing (needs sudo; uses a stand-in daemon that
prints its uid/gid):

```console
$ just jailer-privdrop-check
```

Restrictive sandbox tests cover config-only, cold boot, vsock/MMDS, snapshots,
and both pool flavors. See [Testing](../development/testing.md).

## Security boundary

The current jailer does not provide:

- chroot or mount namespaces;
- Linux cgroups or seccomp;
- a claim that escaped guest code is contained from other local tenants.

What it does provide:

- **uid/gid isolation** — the daemon runs as an unprivileged user after
  privilege drop (`--uid`, `--gid`, `--user`).
- **launchd supervision** — automatic restart on crash via generated plist
  (`--generate-launchd-plist`).
- **macOS sandbox** — deny-by-default filesystem profile.
- **Resource limits** — `--rlimit-nofile`, `--rlimit-nproc`, `--rlimit-fsize`.

Virtualization.framework remains the primary guest/host boundary. The macOS
sandbox narrows the daemon's filesystem and process access, but profile
generation and lifecycle controls are not a complete multi-tenant design.

## Direct sandbox hook

`hephaestus-firecracker --sandbox-profile <file>` applies a caller-supplied
profile before binding the API socket. This low-level hook exists for tests and
custom supervisors. The caller must allow every kernel, rootfs, socket, log,
metrics, snapshot, and pool path that the API may use.
