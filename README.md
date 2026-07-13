# Hephaestus

**Firecracker-compatible microVMs on Apple Silicon.**

Hephaestus implements the Firecracker HTTP API on top of Apple's
Virtualization.framework. Existing Firecracker clients can configure and run
Linux microVMs on macOS by pointing at a Hephaestus UNIX socket.

[![CI](https://github.com/hephaestus-vm/hephaestus/actions/workflows/ci.yml/badge.svg)](https://github.com/hephaestus-vm/hephaestus/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](LICENSE)
[![macOS 26+](https://img.shields.io/badge/macOS-26%2B-success)](https://www.apple.com/macos/)

> [!WARNING]
> Hephaestus is alpha software for trusted workloads. Public interfaces may
> change before v1.0, and untrusted or mutually untrusted guests are not yet
> supported. Read the [security policy](SECURITY.md) before deployment.

## Why Hephaestus?

Firecracker's API is widely used by microVM orchestrators, but Firecracker
itself requires Linux and KVM. Hephaestus preserves the control-plane model on
an Apple Silicon Mac while replacing KVM with Virtualization.framework.

| | Firecracker | Hephaestus |
| :-- | :-- | :-- |
| Host | Linux/KVM | macOS/Virtualization.framework |
| Architectures | x86_64 and aarch64 | Apple Silicon |
| Control plane | HTTP over a UNIX socket | Firecracker-compatible HTTP over a UNIX socket |
| VM per process | Yes | Yes |
| Snapshot format | Firecracker-specific | Virtualization.framework-specific |
| Untrusted multi-tenancy | Firecracker jailer | Not currently supported |

Compatibility applies to API wire shapes and supported behavior. Hephaestus is
not a binary port, and snapshots cannot move between the two hypervisors. See
[Firecracker compatibility](docs/firecracker-compatibility.md) for endpoint and
field-level details.

## Getting started

On an Apple Silicon Mac running macOS 26 or later, download and inspect the
installer, then choose an explicit release version:

```console
$ curl --proto '=https' --tlsv1.2 -fLo install.sh \
    https://raw.githubusercontent.com/hephaestus-vm/hephaestus/main/install.sh
$ less install.sh
$ sh install.sh --version v0.4.0-alpha.1
$ "$HOME/.local/bin/hephaestus" --version
```

The installer verifies the archive checksum, signatures, and virtualization
entitlements before copying all three binaries to `~/.local/bin`. A source build
additionally requires Xcode 26, Rust, `just`, and
[`apple/container`](https://github.com/apple/container). For manual or
system-wide installation, source builds, guest artifacts, and troubleshooting,
see [Getting started](docs/getting-started.md).

## Use the Firecracker API

Build the workspace and inspect the CLI:

```console
$ cargo build --workspace
$ ./build/cargo_target/debug/hephaestus --help
```

Then start one API process:

```console
$ ./build/cargo_target/debug/hephaestus-firecracker \
    --api-sock /tmp/hephaestus.sock \
    --id example
```

In another terminal, configure it with Firecracker HTTP requests:

```console
$ curl --unix-socket /tmp/hephaestus.sock \
    http://localhost/

$ curl --unix-socket /tmp/hephaestus.sock \
    -X PUT http://localhost/machine-config \
    -H 'Content-Type: application/json' \
    -d '{"vcpu_count":2,"mem_size_mib":512}'
```

A complete boot requires a Linux kernel and ext4 root filesystem. Follow the
[Firecracker API guide](docs/guides/firecracker-api.md), or run
`just fc-compat` to exercise the full sequence with the real Firecracker Go
SDK.

## How it works

```mermaid
flowchart TB
    Clients["Firecracker clients"]
    CLI["hephaestus CLI"]
    API["hephaestus-firecracker<br/>HTTP/1.1 over a UNIX socket"]
    Backend["Firecracker lifecycle backend"]
    Containerization["Apple Containerization"]
    Bridge["Rust ↔ Swift bridge"]
    VZ["Apple Virtualization.framework"]
    Guest["arm64 Linux VM"]

    Clients --> API
    API --> Backend
    Backend --> Bridge
    CLI -->|run| Containerization
    CLI -->|vz-*| Bridge
    Containerization --> Bridge
    Bridge --> VZ
    VZ --> Guest
```

Hephaestus has two VM paths:

- `hephaestus run` uses Apple's Containerization library for process-oriented
  execution, terminal handling, and networking.
- `hephaestus-firecracker` and the `vz-*` CLI commands use
  Virtualization.framework directly, enabling snapshots, warm pools, and the
  Firecracker lifecycle.

Both paths share a Rust-to-Swift bridge. The HTTP daemon runs one VM per process,
matching Firecracker's process model. Read the [architecture](docs/design/architecture.md)
for the component and state-machine details.

## Capabilities and limitations

| Area | Current state |
| :-- | :-- |
| Firecracker API | Core lifecycle and the 14-call Go SDK sequence pass |
| Guest networking | Virtualization.framework NAT; guest configures L3 |
| Snapshots | Save and restore between Hephaestus processes |
| Warm pools | Agent and stock-init flavors supported |
| MMDS | Control-plane API plus guest agent/vsock shim |
| Cross-hypervisor snapshots | Unsupported by the underlying formats |
| CPU templates, pmem, memory hotplug | Unsupported by Virtualization.framework |
| Untrusted multi-tenancy | **Unsupported**; jailer remains experimental |
| API stability | Breaking changes may occur before v1.0 |

The canonical support matrix is
[docs/firecracker-compatibility.md](docs/firecracker-compatibility.md).

## Performance

On the current M-series reference system with an Alpine 3.20 guest configured
with 2 vCPUs and 512 MiB of memory:

| Path | Median restore time |
| :-- | --: |
| Agent warm pool | 253.0 ms |
| Stock-init warm pool | 243.4 ms |
| Snapshot load | 234.7 ms |

Approximately 90% of restore time is spent inside Apple's
`restoreMachineStateFrom:` primitive. See [Performance](docs/performance.md) for
the methodology, phase breakdown, and reproduction commands.

## Documentation

- [Documentation index](docs/README.md)
- [Getting started](docs/getting-started.md)
- [Firecracker compatibility](docs/firecracker-compatibility.md)
- [CLI guide](docs/guides/cli.md)
- [Firecracker API guide](docs/guides/firecracker-api.md)
- [Networking and MMDS](docs/guides/networking.md)
- [Snapshots](docs/guides/snapshots.md)
- [Warm pools](docs/guides/warm-pools.md)
- [Jailer](docs/guides/jailer.md)
- [Architecture](docs/design/architecture.md)
- [Contributor setup](docs/development/setup.md)

## Releases

Release artifacts are published through the
[GitHub Releases](https://github.com/hephaestus-vm/hephaestus/releases) page.
Hephaestus follows Semantic Versioning, with additional instability allowed
while the major version is zero. See the [release policy](docs/project/release-policy.md),
[automation plan](docs/project/release-automation.md), and
[changelog](CHANGELOG.md).

## Contributing

Contributions are welcome; no CLA is required. Read
[CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request.

## Security

Do not report vulnerabilities in a public issue. Follow the private reporting
instructions in [SECURITY.md](SECURITY.md).

## Acknowledgments

Hephaestus builds on [Firecracker](https://github.com/firecracker-microvm/firecracker),
[apple/containerization](https://github.com/apple/containerization),
[apple/container](https://github.com/apple/container), and the
[Firecracker Go SDK](https://github.com/firecracker-microvm/firecracker-go-sdk).
Full attribution is in [NOTICE](NOTICE).

## License

Licensed under the [Apache License 2.0](LICENSE).
