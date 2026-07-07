# Dev environment for M4 (jailer) and M1b (vmnet MMDS)

The remaining roadmap milestones are blocked less by code than by **privilege
and entitlements**. This doc records what each needs, what *this* machine can
do today, and the scaffolding added to unblock the work.

## What this machine reports today

| Fact | Value | Implication |
| :-- | :-- | :-- |
| SIP | **enabled** | AMFI enforces restricted entitlements; ad-hoc signing can't grant them |
| `boot-args` | unset | AMFI not disabled |
| Signing identity | **Apple Development: ahmed.adan@gmail.com (VKADA4J3DY)** | Real cert available → the *proper* vmnet path is reachable |
| Provisioning profile | **none installed** | The vmnet entitlement isn't authorized yet |
| `just probe-vmnet` | **`Killed: 9`** (as of this writing) | AMFI refuses vmnet even with the Apple Development cert → **profile required** |
| User / sudo | `aadan` (uid 502); sudo needs a password | Privilege-drop tests need interactive `sudo` |

> **Empirically confirmed:** signing the probe with the Apple Development
> identity is *not* sufficient — AMFI SIGKILLs it. M1b is blocked until a
> Virtualization Networking provisioning profile is installed (or the fallback
> plan is taken). Re-run `just probe-vmnet` after installing one; a printed
> interface list means you're unblocked.

## M1b — MMDS over the guest NIC (bridged vmnet)

Bridged networking (`VZBridgedNetworkDeviceAttachment`) is what lets the host
answer `169.254.169.254` on the guest's L2 — the base-entitlement NAT path
can't (it's a black box; that's why the agent shim exists). It requires the
**restricted `com.apple.vm.networking` entitlement**, which under SIP is only
honored when the code signature is backed by a provisioning profile carrying
the *Virtualization Networking* capability.

### Is it even feasible here? Probe first.

```
just probe-vmnet
```

Compiles `scripts/vmnet-probe.swift`, signs it with your Apple Development
identity + `hephaestus-vmnet.entitlements`, and runs it:

- **`Killed: 9` / no output** → AMFI refused the entitlement for this
  signature. You need the provisioning profile (below). M1b can't run yet.
- **Prints interfaces** → the entitlement is honored; M1b is viable. A
  non-empty bridgeable-interface list means bridged networking works.

### Making it authorized (one-time, account holder only)

1. In the Apple Developer portal, register/edit an **App ID** for the
   firecracker binary's bundle id and enable the **Virtualization Networking**
   capability (Apple grants `com.apple.vm.networking` on request for dev
   accounts).
2. Create + download a **Development provisioning profile** for that App ID and
   install it (`~/Library/MobileDevice/Provisioning Profiles/`).
3. Build signed with the vmnet entitlement + your identity:
   ```
   just sign-vmnet          # or: HEPHAESTUS_SIGN_IDENTITY=<hash> just sign-vmnet
   ```
   The signing hook honors `HEPHAESTUS_ENTITLEMENTS` and
   `HEPHAESTUS_SIGN_IDENTITY` (defaults stay base + ad-hoc, so ordinary
   `cargo build` is unchanged).

### If you can't get the profile

Two fallbacks, in preference order:
1. **Keep the agent-shim MMDS** (already shipped) as the base-entitlement
   default and gate the vmnet path behind runtime entitlement detection —
   write M1b so it *tries* vmnet and falls back cleanly. This lets M1b code
   merge and be exercised on an authorized machine without breaking anyone.
2. Disabling AMFI (`csrutil` + `amfi_get_out_of_my_way=1` boot-arg, via
   Recovery) makes ad-hoc entitlements honored. **Not recommended** — it
   weakens the whole machine and isn't CI-safe. Document-only.

### Recommended code shape for M1b

Detect authorization at runtime and degrade gracefully, so a single build runs
everywhere:

```
if bridgedNetworkingAvailable() {   // entitlement honored + a bridgeable NIC
    attach VZBridgedNetworkDeviceAttachment; run host_mmds on 169.254.169.254
} else {
    fall back to NAT + the guest agent MMDS shim (today's behavior)
}
```

## M4 — jailer productionization (privilege drop, limits, launchd)

Process-group ownership + signal forwarding already landed (a killed jailer
reaps its daemon). What remains needs *root* to be meaningful.

### uid/gid drop

A `--user <name>` (or `--uid/--gid`) flag on the jailer that `setgid`+`setuid`
drops **after** sandbox/profile setup but **before** `exec`. Dropping to a
*different* user requires the jailer to start as root.

Test path on this machine:
1. Create a dedicated service user once (interactive sudo):
   ```
   sudo sysadminctl -addUser _hephaestus -home /var/empty -shell /usr/bin/false
   # or dscl for finer control of uid/gid
   ```
2. Run the jailer under sudo, dropping to it:
   ```
   sudo ./hephaestus-jailer --user _hephaestus --id t --kernel … --rootfs …
   ```
3. Assert the daemon runs as the target uid: `ps -o user,pid,comm -p <pid>`.

When *not* root (the common dev case), the drop should be a validated no-op
(warn if `--user` names a different uid than the current one) so the code path
is still exercisable without sudo.

### Resource limits — ✅ done

The jailer takes `--rlimit-nofile`, `--rlimit-nproc`, and `--rlimit-fsize` and
applies them via `setrlimit` in the same `pre_exec` as `setpgid` (async-signal-
safe; only lowers, so no privilege needed). Verified root-free by
`just jailer-rlimit-check` (a stand-in daemon reports the applied `ulimit`).
(macOS has no cgroups; per-VM cpu/memory caps are already enforced by VZ's
`cpuCount`/`memorySize`.)

### launchd supervisor

For "owns N VMs, restarts on crash," a **user LaunchAgent**
(`~/Library/LaunchAgents/…`, `launchctl bootstrap gui/$(id -u) …`) needs no
root and is testable here; a system LaunchDaemon (`/Library/LaunchDaemons`)
needs root. Start with a user agent for the dev/test loop.

## Scaffolding added by this change

- `hephaestus-vmnet.entitlements` — base + `com.apple.vm.networking`.
- `scripts/link-and-sign.sh` — now honors `HEPHAESTUS_ENTITLEMENTS` /
  `HEPHAESTUS_SIGN_IDENTITY` (defaults unchanged).
- `scripts/vmnet-probe.swift` + `just probe-vmnet` — feasibility probe.
- `just sign-vmnet` — build signed for bridged networking.

## Suggested order

1. `just probe-vmnet` — learn whether M1b is viable here before writing code.
2. M4 rlimits (no root) → uid-drop (needs the service user + sudo) → user
   LaunchAgent.
3. M1b only once the probe is green (or explicitly on the fallback plan).
