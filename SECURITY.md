# Security

## Threat model

hephaestus runs **trusted guest code**. There is no full jailer and no
multi-tenant isolation story yet. `hephaestus-firecracker` has an
experimental `--sandbox-profile <file>` hook, and the repository includes
restrictive sandbox smoke tests, but profile generation, supervisor-owned
process launch, signing/entitlement distribution, and release-gated real-Mac
coverage are not complete. Do not treat this as support for hostile or
mutually-untrusted guests.

Beyond an optional caller-supplied macOS sandbox profile, hephaestus does not
harden against a hostile guest escaping into the host kernel beyond what
Apple's Virtualization.framework provides by default. Multi-tenant use is
explicitly out of scope for the current alpha — if you need to run code you
don't control, you need a different tool or a future hephaestus release with
jailer support.

What we do rely on:

- **Ad-hoc code signing** with the `com.apple.security.virtualization`
  entitlement. `scripts/link-and-sign.sh` applies it at build time.
- **Apple's Virtualization.framework** — VZ is the primary isolation boundary
  between guest kernel and host user processes. Guest-to-host escape through
  VZ would be a VZ bug; report those to Apple via their standard channel.
- **Optional macOS sandbox profiles** for `hephaestus-firecracker` when the
  caller supplies `--sandbox-profile`. This is a hardening primitive, not a
  complete jailer.

Guest-visible services:

- The bundled `hephaestus-agent` starts a guest-side link-local MMDS shim by
  default for controlled images. It listens inside the guest on
  `169.254.169.254:80` and forwards to the host's MMDS vsock service on port
  `16992`. This intentionally exposes configured metadata to guest processes;
  do not put host secrets in MMDS unless every process in the guest may read
  them. Disable it with `HEPHAESTUS_MMDS_SHIM=0` or `hephaestus.mmds=off`.

Out of scope for hephaestus threat-model purposes:

- Timing or side-channel attacks between concurrent guests on the same host.
- Denial of service caused by a guest consuming host memory or CPU.
- Anything involving untrusted HTTP clients on the API socket. The socket is
  UNIX-domain, local-only, and expected to be reachable only by the user who
  owns it.

## Reporting a vulnerability

Please report security issues **privately**, not via public GitHub
issues. Email: **aadan@nodegroup.ca**.

What helps:

- A description of the issue and its impact.
- A minimal reproducer if possible.
- The commit SHA you're testing against.

Expected timeline during alpha:

- Acknowledgement within 72 hours.
- Initial assessment within 1 week.
- Fix + coordinated disclosure within 90 days, or on a schedule we
  agree with the reporter.

No bug bounty (alpha project), but happy to credit reporters in
release notes.

## Supported versions

Only the most recent release gets security fixes during alpha. Once
we reach v1.0 we'll establish a proper support window.
