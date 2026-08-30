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

## The Virtualization.framework boundary

The guest/host boundary is hardware virtualization — the same *kind* of
mechanism as KVM. What differs is the assurance behind it:

**What Apple stands behind.** A guest-to-host escape from
Virtualization.framework is a critical, bounty-eligible vulnerability class
that Apple patches and assigns CVEs for. Apple stakes its own products on
this boundary: [apple/container](https://github.com/apple/container) runs
each Linux container in its own VZ VM precisely for "the isolation
properties of a full VM," and Apple's Private Cloud Compute [Virtual
Research Environment](https://security.apple.com/blog/pcc-security-research/)
runs PCC node software — the platform behind Apple's strongest public
security claims — in a virtual machine on Apple Silicon Macs.

**What Apple does not provide.** No published threat model or multi-tenancy
endorsement for VZ, no source, and no adversarial track record at hostile
scale. The contrast with PCC is instructive: there, Apple publishes a
security guide, an append-only transparency log with binary images for
independent inspection, partial source, a research environment, and a
dedicated bounty — proof that Apple *can* make a component auditable when it
intends a security posture. VZ has none of that, and independent analysis of
PCC ([arXiv 2605.24239](https://arxiv.org/abs/2605.24239)) found even that
program stops short of full verifiability (no reproducible builds, no
symbols).

Hephaestus therefore treats VZ as a real but **unauditable** boundary:
strong enough to anchor the trusted-workload model and the contained-tenant
ceiling described below, never strong enough to support hostile
multi-tenancy claims. Escapes through VZ are Apple's bugs; report them to
Apple.

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
| `.uid-allocations` (shared uid registry) | root | no |

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
are unreachable (dedicated per-VM uids under `--uid-base`, or at minimum
different `0700` directories; the jailer refuses two live instances on one
uid unless explicitly allowed), and the pool base is read-only.

### The network segment

Cross-VM reachability is **measured, not inferred** — the permanent smokes
`just net-isolation-check` (NAT) and `just vmnet-isolation-check` (vmnet)
boot two VMs and assert that neither TCP nor ICMP crosses between them,
with a gateway-reachability control proving the probe itself works. As
measured on macOS 26.5.2:

- **VZ NAT**: both guests lease addresses in one `192.168.64.0/24`, and
  still cannot reach each other — VZ's opaque NAT fences guests despite
  the shared subnet. VZ documents none of this, so the claim rests on the
  measurement; the smoke exists to catch an OS update changing it.
- **vmnet**: each daemon creates its own network object, which vmnet
  assigns a distinct `/24` under `192.168/16`; guests cannot reach each
  other. A multi-VM *shared* vmnet segment is not constructible in
  Hephaestus at all: `VZVmnetNetworkDeviceAttachment` requires the network
  object to be created in the attaching process, and Hephaestus runs one
  process per VM. Shared segments are a non-feature — the only sanctioned
  future path is an explicit serialized-network handoff.

The transparent MMDS responder serves the daemon's own single metadata
document; per-instance scoping is a consequence of one process per VM, not
of request keying.

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

## Claims

What Hephaestus claims **today**:

1. **A compromised daemon is contained to its own VM's assets.** Post-drop,
   a fully hostile daemon cannot write the pool base, reach another
   instance's work dir, replace any file root re-reads or re-executes
   against, or redirect a privileged operation through a symlink. The
   shared-resources sections above substantiate this path by path.
2. **Co-resident local users get nothing.** Work-root modes and ownership
   checks deny read, plant, and redirect attacks from other uids.
3. **The lifecycle is operationally sound.** No double launches or socket
   stealing, claims survive supervisor crashes, supervised restarts
   preserve state, and retirement is complete.
4. **The guest/host boundary is hardware virtualization**, with the
   assurance caveats stated above.

The **target tier** — not yet claimed — is *contained semi-trusted
tenants*: the posture of Apple's own `container` project (one VM per
workload behind the VZ boundary), which Hephaestus already exceeds on
host-side confinement. Per-VM dedicated uids now exist: `--uid-base`
allocates each instance its own uid/gid (persisted in the root-owned
`.uid-allocations` registry), and the jailer refuses to launch two live
instances under one uid regardless of drop mode (`--allow-shared-uid`
opts out). Per-VM networking is measured and configured: both NAT and
vmnet are cross-VM isolated (see the network-segment section), vmnet
subnets are pinned per instance, the host-side packet interface exists
only for the MMDS responder, and the jailer forwards the network mode.
Claiming the tier still requires one thing: a review of the generated
sandbox profile's non-filesystem allow surface.

## Non-claims

Hephaestus does **not** claim — at any tier, including the target tier:

- containment of **hostile or mutually untrusted** guests. Firecracker's
  version of that claim rests on an open, independently audited boundary
  under years of adversarial pressure at cloud scale; none of that is
  reproducible on a closed boundary.
- protection against a root-level host attacker;
- resistance to **denial of service** by a guest — macOS has no cgroups
  equivalent, and rlimits cannot cap a daemon's memory or CPU;
- isolation against cross-guest **timing and side channels**.

Virtualization.framework remains the primary guest/host boundary in all
cases.
