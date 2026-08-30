# Jailer

`hephaestus-jailer` is a per-VM supervisor. It prepares a private working directory, generates a deny-by-default macOS sandbox profile, and starts one `hephaestus-firecracker` process under that profile.

> [!WARNING]
> This is hardening work in progress, not support for hostile or mutually
> untrusted guests. Read [SECURITY.md](../../SECURITY.md).

## What it does

Before launching the daemon, the jailer:

1. Validates the VM identifier as one safe path component.
2. Claims the instance with an exclusive lock on `<work-dir>/.<id>.lock`, so
   a second jailer for the same id is refused instead of silently stealing
   the live API socket.
3. Creates a private `<work-dir>/<id>/` directory and removes a stale
   `api.sock` left by a previous run.
4. Canonicalizes the kernel, rootfs, initramfs, pool, and daemon paths.
5. Generates a sandbox profile allowing required framework access, the VM's
   inputs, and its work directory.
6. Places the API socket under the per-VM work directory and keeps the
   generated profile in the root-owned parent.
7. Applies optional file-descriptor, process, and file-size limits.
8. Optionally drops privileges to an unprivileged uid/gid (requires root),
   handing the per-VM work dir to the target user while keeping the generated
   sandbox profile root-owned outside that writable directory.
9. Starts the daemon in a process group and forwards termination signals.

## Lifecycle

Each instance id owns three kinds of on-disk state under the work root: the
per-VM work dir `<work-dir>/<id>/` (API socket, logs, metrics, snapshots),
and the root-owned dot-siblings `.<id>.sandbox.profile` and `.<id>.lock`
(plus `.<id>.launchd.*.log` under launchd, and a line in the shared
`.uid-allocations` registry when the instance drops privileges). The
dot-siblings deliberately live outside the daemon-writable work dir so a
compromised dropped-uid daemon cannot replace them.

The instance lock is a `flock` held for the life of the supervised daemon.
The lock file descriptor is inherited by the daemon, so the claim survives
even a `SIGKILL`ed jailer for as long as the daemon runs — a relaunch cannot
steal a live socket. The lock releases automatically when both processes
exit; a crashed instance needs no stale-lock recovery.

By default everything in the work dir persists across restarts, which is
what a `KeepAlive` launchd job wants: snapshots and logs survive. Two
opt-in operations manage that state:

- `--clean-work-dir` empties the per-VM work dir before launch — a one-shot
  retire-and-recreate. It is never carried into generated launchd plists,
  so supervised restarts always preserve state.
- `--teardown` (with `--id` and `--work-dir`) removes the instance's
  on-disk state entirely — work dir, profile, launchd logs, uid-registry
  entry, and lock — and exits without launching. It refuses while the instance is running. Under
  launchd, run `sudo launchctl bootout system/com.hephaestus.vm.<id>`
  first; a still-loaded `KeepAlive` job would simply re-create everything.

```console
$ sudo hephaestus-jailer --id example --teardown
```

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

### Per-VM dedicated uids (`--uid-base`)

When running more than one VM, prefer `--uid-base <n>` over a shared
`--user nobody`: two daemons under one uid can signal, ptrace, and access
each other's work dirs, so a shared uid collapses instance separation back
onto the sandbox profile alone.

```console
$ sudo hephaestus-jailer \
    --id example \
    --kernel /absolute/path/to/vmlinux \
    --rootfs /absolute/path/to/rootfs.ext4 \
    --uid-base 61000
```

Each instance id is allocated the lowest free uid in `[base, base+1000)` —
skipping uids taken by other instances (even retired-but-not-torn-down
ones) and by real user accounts — with gid set equal to the uid. The
allocation is recorded in the root-owned `<work-dir>/.uid-allocations`
registry and reused on every relaunch, so an id keeps its uid (and the
ownership of its chowned files) across restarts; `--teardown` releases the
entry. Generated launchd plists pin the allocated `--uid`/`--gid` rather
than carrying `--uid-base`, so supervised restarts never re-run allocation.

Whatever the drop mode, the jailer refuses to launch while another *live*
instance runs under the same uid, naming both instances in the error. Pass
`--allow-shared-uid` for deliberate shared-uid deployments (it propagates
into generated plists).

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

The job's stdout/stderr land in root-owned `.<id>.launchd.stdout.log` and
`.<id>.launchd.stderr.log` next to the profile in the work root — outside
the daemon-writable work dir, because launchd reopens these paths as root on
every restart. launchd does not rotate them; for a long-lived VM add a
`newsyslog.d` entry, e.g.:

```
# /etc/newsyslog.d/hephaestus-example.conf
/tmp/hephaestus-jail/.example.launchd.stdout.log  644  5  1024  *  NJ
/tmp/hephaestus-jail/.example.launchd.stderr.log  644  5  1024  *  NJ
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

`--network-backend {nat|vmnet}` and `--host-mmds` are forwarded to the
daemon. For vmnet, `--firecracker-binary` must point at the
profile-authorized app-bundle binary
(`build/HephaestusFirecracker.app/Contents/MacOS/hephaestus-firecracker`);
the jailer cannot detect the entitlement itself, so validate with
`just probe-vmnet` before installing a launchd plist, which would otherwise
crash-loop. `--host-mmds` without vmnet is refused before anything touches
the filesystem.

Validate resource-limit plumbing without booting a VM:

```console
$ just jailer-rlimit-check
```

Validate privilege-drop plumbing (needs sudo; uses a stand-in daemon that
prints its uid/gid):

```console
$ just jailer-privdrop-check
```

Validate lifecycle plumbing — instance-lock refusal, stale-socket removal,
and teardown (root-free, VM-free):

```console
$ just jailer-lifecycle-check
```

Validate per-VM uid allocation — distinct uids per id, the live-shared-uid
refusal, and registry release (needs sudo; VM-free):

```console
$ just jailer-uid-check
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
  privilege drop (`--uid`, `--gid`, `--user`), with per-VM dedicated uids
  via `--uid-base` and a refusal to launch two live instances on one uid.
- **launchd supervision** — automatic restart on crash via generated plist
  (`--generate-launchd-plist`).
- **macOS sandbox** — deny-by-default filesystem profile.
- **Resource limits** — `--rlimit-nofile`, `--rlimit-nproc`, `--rlimit-fsize`.
- **Lifecycle ownership** — a per-instance lock refuses double launches, and
  the jailer owns stale-socket cleanup and state retirement (`--teardown`).

Virtualization.framework remains the primary guest/host boundary. The macOS
sandbox narrows the daemon's filesystem and process access, but profile
generation and lifecycle controls are not a complete multi-tenant design.

## Direct sandbox hook

`hephaestus-firecracker --sandbox-profile <file>` applies a caller-supplied
profile before binding the API socket. This low-level hook exists for tests and
custom supervisors. The caller must allow every kernel, rootfs, socket, log,
metrics, snapshot, and pool path that the API may use.
