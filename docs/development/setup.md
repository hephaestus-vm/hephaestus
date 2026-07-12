# Development setup

This guide prepares a supported Mac for work on the Rust, Swift, and guest Linux
parts of Hephaestus.

## Host requirements

- Apple Silicon and macOS 26+
- Xcode 26+ selected with `xcode-select`
- Rust toolchain from `rust-toolchain.toml`
- `just`
- `apple/container` for real-VM artifacts
- Go for work on `compat/firectl-harness`

```console
$ brew install rustup just container go
$ rustup-init
$ container system start
$ container run --rm docker.io/library/alpine:3.20 echo ready
```

## Build the workspace

```console
$ git clone https://github.com/hephaestus-vm/hephaestus
$ cd hephaestus
$ cargo build --workspace
$ cargo test --workspace
```

Cargo builds the Swift package through `hephaestus-bridge/build.rs` and signs
binaries through `scripts/link-and-sign.sh`. Build products are redirected to
`build/cargo_target` by the repository Cargo configuration.

Useful checks:

```console
$ just ping
$ just verify-signing
$ just artifacts
```

## Guest agent

Guest-side changes require the arm64 musl target and the repository's
cross-compilation tooling:

```console
$ rustup target add aarch64-unknown-linux-musl
$ just build-agent
```

The generated initramfs is `build/agent.cpio.gz`.

## Repository map

- `src/` — Rust workspace crates
- `swift/HephaestusBridge/` — SwiftPM package and VZ implementation
- `guest/hephaestus-agent/` — Linux guest agent
- `compat/firectl-harness/` — Go SDK compatibility client
- `scripts/` and `justfile` — executable development workflows
- `vendor/firecracker/` — upstream reference source, excluded from Cargo

Read the [architecture](../design/architecture.md) before changing a boundary
between these components.

## Ordinary versus privileged development

The normal build uses only `com.apple.security.virtualization` and does not need
sudo or an Apple Developer provisioning profile. Bridged vmnet work and
cross-user jailer tests have additional requirements; keep them out of the base
development loop and follow [Privileged features](privileged-features.md).
