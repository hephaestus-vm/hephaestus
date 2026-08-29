# Threat model: shared host resources

This document states what the Hephaestus jailer defends, against whom, and
what it explicitly does not claim. It covers the resources that multiple
parties on one host can reach: the work root, the warm pool, snapshots, the
network segment, and MMDS metadata. The summary in
[SECURITY.md](../../SECURITY.md) defers to this document for detail.

## Tenant model

Hephaestus supports **one operator running trusted guest workloads** on a
host they control. The jailer's hardening limits the blast radius of a
*compromised* daemon or guest; it is not an isolation boundary between
*mutually untrusted* tenants. If you need to run code you don't control,
you need a different tool.

## Trust zones

From most to least privileged:

1. **Root / the invoking operator.** Runs the jailer, owns the work root and
   its dot-sibling control files, loads launchd jobs.
2. **launchd.** Re-runs the whole jailer (as root) on every `KeepAlive`
   restart and reopens the job's log paths as root.
3. **The daemon (`hephaestus-firecracker`).** After privilege drop it runs
   as an unprivileged uid/gid under a deny-by-default sandbox profile, owning
   only its per-VM work dir, the rootfs file, and any granted pool slots.
4. **The guest.** Confined by Virtualization.framework. Sees its devices,
   the MMDS shim, and whatever network attachment it was configured with.
5. **Other local users.** No intended access to any Hephaestus state.

## Adversaries considered

- **A1 — a compromised daemon**: an attacker who fully controls a
  `hephaestus-firecracker` process *after* privilege drop and sandbox entry
  (e.g. via a bug in the HTTP backend or a VZ escape into the daemon
  process).
- **A2 — a co-resident unprivileged local user** on the same host.
- **A3 — a hostile guest**: only insofar as it becomes A1 by escaping VZ.
  Guest-to-host escape through Virtualization.framework itself is a VZ bug
  and out of Hephaestus's hands; report it to Apple.

Untrusted API clients are **not** an adversary: the API socket is
UNIX-domain, mode-protected, and expected to be reachable only by its owner.
Denial of service (a guest burning host CPU/memory/disk) and cross-guest
side channels are out of scope.

## Shared resources and their defenses

### The work root and its control files

The work root (`$TMPDIR/hephaestus-jail` by default, `--work-dir` otherwise)
is created root-owned with mode `0700` (`0711` when a privilege drop needs
the dropped uid to traverse it) and is re-verified on **every** launch and
teardown: a symlinked root, a non-directory, or a root owned by another uid
is refused outright. This defeats A2 pre-planting a root under
world-writable `/tmp` to redirect the sandbox grant or the jailer's
descent.

Per instance `<id>` (validated to Firecracker's charset, so it can never
traverse), the root holds:

| Path | Owner | Writable by A1? |
| :-- | :-- | :-- |
| `<id>/` (api socket, logs, metrics, snapshots) | dropped uid after chown | yes — this is A1's home |
| `.<id>.sandbox.profile` | root | no |
| `.<id>.lock` | root | no (fd inherited, path not writable) |
| `.<id>.launchd.stdout.log`, `.<id>.launchd.stderr.log` | root | via inherited fd only |

The invariant: **everything root re-reads or re-executes against lives
outside the daemon-writable directory.** A1 owns the contents of `<id>/`
and nothing else. Concretely:

- A1 cannot swap the sandbox profile before the next root launch
  regenerates it — the profile is root-owned in a directory A1 cannot
  write, and the jailer rewrites and re-modes it on every run.
- A1 cannot make launchd (root) append to an arbitrary path: the plist's
  log paths are root-owned dot-siblings, and launchd's reopen-on-restart
  never traverses A1-writable directories. A1 can only write log *content*
  through the inherited descriptors.
- A1 cannot release or steal the instance claim by path: `--teardown` and a
  second launch both go through the root-owned lock file. A1 *can* unlock
  the inherited lock fd it holds; the lock is an operational
  double-launch/teardown guard among cooperating supervisors, not a
  boundary against A1 (which already owns everything the lock protects).
- A leaf symlink at `<root>/<id>` is refused on launch, `--clean-work-dir`,
  and `--teardown`, so neither the profile grant nor a recursive delete can
  be redirected into a victim directory.

A2 never gets past the root directory's mode: `0700` blocks everything, and
`0711` allows traversal only to paths A2 would still need to own or read,
none of which are readable (`<id>/` is `0700` dropped-uid-owned; the
dot-siblings are root-owned with no group/other read except the profile's
deliberate `0644`, which contains only paths, not secrets).

### Warm pools

The pool base (`save.bin`, `pristine.ext4`, machine id, metadata) is granted
**read-only** to the daemon; only pre-created `slot-*` directories are
read/write, and slot claims are exclusive `flock`s. A1 can therefore
corrupt *its own claimed slot* (a per-instance asset) but cannot overwrite
the golden snapshot every other instance restores from. Pool slots are the
one deliberately shared-writable surface between instances **over time**:
a slot previously used by a compromised instance is reset from the
read-only pristine base on the next claim, which is what makes that safe.

### Snapshots

Snapshot files live in the per-VM work dir, so their integrity equals the
daemon's integrity: A1 can feed itself a malicious snapshot, which changes
nothing (it already controls the VM). What the layout prevents is A1
poisoning *another* instance's restore path — other instances' work dirs
are unreachable (different dropped uids or at minimum different `0700`
directories), and the pool base is read-only.

### The network segment

NAT attachments give each VM a VZ-managed private network. The shared-vmnet
path intentionally puts participating VMs **on one L2 segment**: guests can
reach each other, and nothing in Hephaestus claims otherwise. Under the
trusted-workload tenant model that is a feature; do not put a guest you
would not let talk to the others on the shared segment. The transparent
MMDS shim on that segment serves per-instance metadata keyed by the
requesting VM, but the segment itself provides no guest-to-guest isolation.

### MMDS metadata

MMDS contents are readable by **every process in the guest** the metadata
is configured for. Treat MMDS as guest-public: no host secrets, nothing you
would not hand to the least-trusted process in that guest.

## Residual risks (known and accepted)

- **A SIGKILLed jailer orphans the daemon** (macOS has no
  `PR_SET_PDEATHSIG`). The inherited lock fd keeps the instance claim held
  so a relaunch cannot double-claim; under launchd the restarted jailer
  will be refused until the orphan exits. Operators kill the orphaned
  process group manually in that case.
- **No chroot, mount namespaces, cgroups, or seccomp analogue.** The
  sandbox profile and rlimits are the only daemon confinement; the profile
  is deny-by-default but macOS sandbox escapes are Apple's boundary, not
  ours.
- **Ad-hoc signing** until signed distribution lands: the entitlement is
  applied locally, and binary provenance relies on the operator's own
  build/install hygiene.
- **launchd logs grow without rotation** (documented, with a `newsyslog.d`
  recipe, in the [jailer guide](../guides/jailer.md)).

## Non-claims

Hephaestus does **not** claim: containment of hostile or mutually untrusted
guests; protection against a root-level host attacker; resistance to
denial of service by a guest; or isolation against cross-guest timing and
side channels. Virtualization.framework remains the primary guest/host
boundary in all cases.
