# Hephaestus documentation

This documentation is organized by what you are trying to accomplish:

- **Start** gets Hephaestus installed and boots a first Linux VM.
- **Guides** cover specific operator workflows.
- **Compatibility** records the Firecracker contract and every known deviation.
- **Design** explains how Hephaestus works internally.
- **Development** covers building, testing, and contributing.
- **Project** records current direction and release policy.

## Start

- [Getting started](getting-started.md) — install prerequisites, build, and boot
  the first VM.
- [Guest images](guides/guest-images.md) — understand kernel, initramfs, and
  root filesystem inputs.

## Guides

- [Firecracker API](guides/firecracker-api.md) — configure and start a VM over
  HTTP on a UNIX socket.
- [CLI](guides/cli.md) — process execution and direct-VZ commands.
- [Networking and MMDS](guides/networking.md) — NAT, guest addressing, metadata,
  and entitlement-dependent networking.
- [Snapshots](guides/snapshots.md) — save and restore VM state.
- [Warm pools](guides/warm-pools.md) — pre-snapshot VMs for faster startup.
- [Jailer](guides/jailer.md) — experimental sandbox supervisor and its security
  boundary.

## Compatibility and performance

- [Firecracker compatibility](firecracker-compatibility.md) — canonical API
  support matrix and behavioral differences.
- [Performance](performance.md) — restore benchmarks and reproduction method.

## Design

- [Architecture](design/architecture.md) — components, VM paths, and lifecycle.
- [Virtualization.framework mapping](design/virtualization-framework.md) — how
  Firecracker concepts map onto Apple's APIs.
- [Rust/Swift FFI](design/ffi.md) — ABI, ownership, and build integration.
- [Guest agent](design/guest-agent.md) — command and metadata channels.
- [Engineering style](design/engineering-style.md) — project design and review
  principles.

## Development

- [Development setup](development/setup.md)
- [Testing](development/testing.md)
- [Compatibility testing](development/compatibility-testing.md)
- [Privileged features](development/privileged-features.md)
- [Contributing](../CONTRIBUTING.md)

## Project

- [Roadmap](project/roadmap.md)
- [Release policy](project/release-policy.md)
- [Changelog](../CHANGELOG.md)
- [Security policy](../SECURITY.md)
