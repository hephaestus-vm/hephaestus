# Security

## Threat model

hephaestus runs **trusted guest code**. There is no jailer, no host
process sandbox, and no hardening against a hostile guest escaping
into the host kernel beyond what Apple's Virtualization.framework
provides by default. Multi-tenant use is explicitly out of scope for
the current alpha — if you need to run code you don't control, you
need a different tool or a future hephaestus release with jailer
support (not yet built).

What we do rely on:

- **Ad-hoc code signing** with the `com.apple.security.virtualization`
  entitlement. `scripts/link-and-sign.sh` applies it at build time.
- **Apple's Virtualization.framework** — VZ is the isolation boundary
  between guest kernel and host user processes. Guest-to-host escape
  would be a VZ bug; report those to Apple via their standard channel.

Out of scope for hephaestus threat-model purposes:

- Timing or side-channel attacks between concurrent guests on the
  same host.
- Denial of service caused by a guest consuming host memory or CPU.
- Anything involving untrusted HTTP clients on the API socket (the
  socket is UNIX-domain, local-only, and expected to be reachable
  only by the user who owns it).

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
