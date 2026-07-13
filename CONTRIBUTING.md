# Contributing to Hephaestus

Thanks for helping build a Firecracker-compatible VMM for macOS. Hephaestus is
alpha software, so small, testable changes are easier to review and safer to
ship.

> Looking for upstream Firecracker contribution guidelines? Use the
> [Firecracker repository](https://github.com/firecracker-microvm/firecracker).

## Good first contributions

- Reproducible bug reports for macOS, Xcode, entitlement, or signing behavior
- Firecracker compatibility tests and documented wire-shape differences
- Focused fixes to errors, cleanup, and lifecycle handling
- Documentation and recipe improvements verified on a supported Mac

Open an issue before investing in a large feature or architecture change. This
lets maintainers confirm scope, platform feasibility, and the expected tests.

## Set up the repository

Follow [Development setup](docs/development/setup.md). The short path is:

```console
$ git clone https://github.com/hephaestus-vm/hephaestus
$ cd hephaestus
$ cargo build --workspace
$ cargo test --workspace
```

Real-VM tests additionally need artifacts from `apple/container`. Guest-agent
work requires the `aarch64-unknown-linux-musl` target and `just build-agent`.
Restricted vmnet and cross-user isolation work have separate host requirements
documented under [Privileged features](docs/development/privileged-features.md).

## Before opening a pull request

Run the baseline checks:

```console
$ cargo fmt --all -- --check
$ cargo clippy --workspace --all-targets -- -D warnings
$ cargo test --workspace
$ python3 scripts/check-doc-links.py
```

Then run the tests required by the changed boundary:

- Firecracker routes or wire types: `just fc-compat-config`
- VM construction or devices: the relevant real-VM e2e
- Rust/Swift FFI: `just ping` and a real-VM path
- Agent, vsock, snapshots, pools, sandbox, or jailer: the corresponding smoke
  family

The [testing guide](docs/development/testing.md) maps areas to commands. If a
required test cannot run on your host, say so in the pull request rather than
leaving the test plan ambiguous.

## Compatibility changes

Firecracker compatibility is a user-facing contract. A change that adds,
removes, accepts, ignores, or alters an upstream field must update all applicable
parts in the same pull request:

1. Wire types or route behavior
2. Go SDK compatibility harness
3. Real-VM test when execution semantics change
4. [Compatibility documentation](docs/firecracker-compatibility.md)

Read [Compatibility testing](docs/development/compatibility-testing.md) before
synchronizing a new upstream API version.

## Engineering expectations

The project crosses host/guest, Rust/Swift, HTTP/VMM, socket/vsock, and
sandbox/filesystem boundaries. Keep ownership, blocking behavior, state
transitions, and cleanup explicit. The full review framework is in
[Hephaestus Style](docs/design/engineering-style.md).

Commit messages should:

- use an imperative subject no longer than 79 characters;
- explain why the change is needed;
- keep unrelated changes in separate commits.

`git log --oneline -10` is a useful style reference.

## Review and merge

A maintainer reviews and merges each pull request. Wire compatibility, FFI,
snapshot, and isolation changes receive additional scrutiny because small drift
can break clients or resource ownership. During alpha, nudge the pull-request
thread if there has been no response within a week.

## Documentation

Keep one canonical location for each fact: compatibility in the compatibility
document, security guarantees in `SECURITY.md`, benchmark results in the
performance document, and setup requirements in the start/development guides.
The README should summarize and link rather than duplicate long-form material.

Commands in user guides are part of the product. Test them when changing them,
and update links when moving documentation.

## Code of conduct

Be respectful. Harassment, personal attacks, and discrimination are not
acceptable. Technical disagreement is welcome when it stays focused on the
work and its tradeoffs.
