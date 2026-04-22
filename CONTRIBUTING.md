# Contributing to hephaestus

> Looking for Firecracker contribution guidelines? This repo is a
> macOS port, not upstream. Head to
> [firecracker-microvm/firecracker](https://github.com/firecracker-microvm/firecracker)
> for the upstream project.

Thanks for your interest. hephaestus is alpha software — shapes are
still settling — so the most valuable contributions are:

- **Bug reports** on macOS compatibility, entitlement/signing issues,
  or wire-shape drift against current Firecracker.
- **Small fixes** to docs, error messages, and recipes.
- **Compatibility tests** exercising Firecracker API paths we don't
  yet cover.

If you want to tackle something bigger, please file an issue first so
we can agree on scope before you spend time.

## Developer Certificate of Origin

hephaestus uses the [Developer Certificate of Origin][DCO] — a simple
signed attestation that you have the right to submit the contribution.
Every commit must carry a `Signed-off-by:` line:

```
git commit -s -m "your message"
```

No CLA. No copyright assignment.

[DCO]: https://developercertificate.org/

## Dev setup

Same requirements as the install-from-source path in the
[README](README.md#requirements): Apple Silicon, macOS 26+, Xcode 26,
Rust stable, `apple/container`. Then:

```bash
git clone https://github.com/hephaestus-vm/hephaestus
cd hephaestus
cargo build --workspace
cargo test --workspace
```

If you're touching guest-side code (`guest/hephaestus-agent/`), you
also need rustup with the `aarch64-unknown-linux-musl` target:

```bash
rustup target add aarch64-unknown-linux-musl
just build-agent
```

## Before you open a PR

Run these and make sure they all pass:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

If you touched the HTTP wire layer (`src/hephaestus-fc-api/` or
`src/hephaestus-firecracker/`), also run the compat smoke against the
real Firecracker Go SDK:

```bash
just fc-compat
```

## Commit style

- **Subject ≤ 79 characters.** Count before committing.
- **Imperative mood**: "add snapshot round-trip", not "added".
- **Focus on why**, not what. The diff already shows what changed.

Match the style of recent commits — `git log --oneline -10` is a
reasonable reference.

## Review and merge

One maintainer (@ahmedadan) reviews and merges. During alpha, expect a
response within a week; nudge in the PR thread if longer. Changes
that touch the Firecracker wire layer get extra scrutiny because
drift there breaks real-client compatibility.

## Code of Conduct

Be decent. No harassment, no personal attacks. Disagreements about
technical direction are welcome; ad-hominem isn't.
