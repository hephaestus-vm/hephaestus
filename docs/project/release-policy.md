# Release policy

Hephaestus uses [Semantic Versioning](https://semver.org/) and records notable
changes in the root [changelog](../../CHANGELOG.md). The current workflow is
being replaced by the staged [release automation plan](release-automation.md).

## Alpha releases

While the major version is zero:

- CLI flags, API extensions, pool metadata, and snapshot assumptions may change;
- only the newest release receives security fixes;
- Firecracker wire compatibility is versioned independently and reported by
  `GET /version`;
- release notes must call out snapshot, pool, guest-agent, or configuration
  changes that require regeneration or migration.

The current trusted-workload security boundary applies to every alpha release
unless [SECURITY.md](../../SECURITY.md) explicitly states otherwise.

## Release artifacts

The repository does not yet have an origin version tag or published GitHub
Release. The first automated release will publish ad-hoc-signed arm64 macOS
binaries on [GitHub Releases](https://github.com/hephaestus-vm/hephaestus/releases).
Users must verify the source and artifact before removing macOS quarantine. A
future Developer ID distribution may have different signing and entitlement
behavior.

## Release requirements

A release candidate should pass:

- formatting, clippy, and all workspace tests;
- config-only and restrictive-sandbox Go SDK compatibility tests;
- cold boot, networking, vsock/MMDS, snapshot, and both pool-flavor smokes on a
  supported Mac;
- signing/entitlement verification;
- documentation link and command checks;
- changelog and compatibility-version review.

## Firecracker compatibility version

The version returned by `GET /version` identifies the upstream API snapshot that
wire types and compatibility tests target. It must only change after the
vendored reference, API types, harness, and compatibility documentation are
updated together.

## Toward v1.0

A stable release requires defined support windows for macOS and Xcode, a public
compatibility policy, stable operator-facing flags, and an explicit production
security model. Until then, breaking changes are announced in release notes and
the changelog rather than supported through a long deprecation window.
