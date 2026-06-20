# Hephaestus Style

This document is inspired by TigerBeetle's TigerStyle and adapted for hephaestus:
a Firecracker-compatible macOS VMM built across Rust, Swift, shell, and guest Linux code.

## Why Have Style?

Style is design. For hephaestus, design goals are:

1. **Safety** — never make guest-to-host boundaries, file access, socket bridging, or VM lifecycle
   behavior ambiguous.
2. **Compatibility** — preserve Firecracker wire shapes and client expectations unless a deviation
   is explicit and documented.
3. **Operability** — failures must be diagnosable from logs, errors, and reproducible recipes.
4. **Performance** — keep cold boot, restore, and data-path overhead predictable.
5. **Developer experience** — keep the multi-language stack reviewable and easy to validate.

Readability matters because it serves these goals. A beautiful abstraction that obscures VM state,
sandbox permissions, or FFI ownership is not stylish here.

## Safety

### Boundaries are explicit

Hephaestus crosses several trust and runtime boundaries:

- Host ↔ guest.
- Rust ↔ Swift FFI.
- HTTP API ↔ VMM backend.
- Unix socket ↔ VZ vsock.
- Sandbox profile ↔ host filesystem.
- Guest initramfs ↔ mounted rootfs.

Code crossing a boundary must say what is owned, what is borrowed, what can block, what can fail,
and what remains valid after the call returns.

### Fail closed at isolation boundaries

Sandbox and jailer code should deny by default. Add permissions only when a real e2e proves they are
needed. Prefer narrow files and per-VM work directories over broad temp-directory grants. If a broad
grant is temporarily necessary, document why and what will remove it.

### Assert and validate invariants near the boundary

Use Rust types where possible, but do not rely on types alone for external data. Validate:

- Firecracker API state transitions before acting.
- Paths before storing them in backend state.
- Link-local MMDS addresses before accepting config.
- Snapshot and pool match preconditions before restore.
- FFI return status and out-parameters immediately after calls.

Assertions are for programmer errors; user/API errors must become structured Firecracker-shaped
errors where possible.

### Separate operating errors from programmer errors

Expected failures include missing files, invalid API state, unsupported Firecracker features,
sandbox denials, VM configuration errors, and entitlement failures. Return or report these with
context. Unexpected invariant violations should crash tests or fail fast.

### Keep resource lifetimes obvious

VM handles, pool slots, sockets, file descriptors, serial attachments, and spawned tasks are
resources. Their ownership and cleanup must be visible. In scripts, use `trap` immediately after
resource creation. In Rust/Swift, prefer RAII/defer-style cleanup and keep resource variables in the
smallest useful scope.

### Avoid hidden blocking and deadlocks

Anything involving stdin, vsock, Unix sockets, HTTP serving, VM pause/resume, or process shutdown can
hang. Always consider EOF, half-close semantics, read/write ordering, and teardown races. E2e tests
must exercise these paths with timeouts.

## Compatibility

### Firecracker compatibility is a contract

The Go SDK harness is a first-class test, not a convenience. Wire-shape drift is a bug unless the
compat docs explicitly call out a partial or unsupported behavior.

### Deviations are documented where users look

When behavior differs from Firecracker, update the compatibility table and endpoint notes in the
same change. For architecture-level deviations, update `docs/ARCHITECTURE.md`.

### Path and state semantics matter

Firecracker clients configure resources before `InstanceStart`. Preserve pre-boot/post-boot rules,
error shapes, and idempotency. If VZ cannot support a Firecracker behavior, accept-noop only when
real clients rely on that behavior being accepted.

## Operability

### Every failure needs a next clue

Errors should tell the operator what failed and where to look next: path, socket, VM id, entitlement,
sandbox profile, or recipe. For entitlement-sensitive features, include the missing entitlement or
platform condition in the message.

### Logs live under ownership

Per-VM logs, metrics, sockets, profiles, snapshots, and sidecars should live under a per-VM work
directory whenever possible. This keeps sandbox profiles narrow and cleanup simple.

### Recipes are part of the product

`just` recipes and scripts are executable documentation. They should:

- Discover artifacts using existing project conventions.
- Clone mutable rootfs images before boot.
- Clean up with traps.
- Print artifact/log paths on failure.
- Be explicit when they are local-only and not CI-safe.

## Performance

### Respect the control plane/data plane split

HTTP configuration, metrics, and MMDS are control-plane paths. Vsock bridging and VM execution are
closer to data-plane paths. Keep control-plane compatibility complete without adding avoidable
per-byte overhead to data paths.

### Sketch costs before adding work to boot/restore

Cold boot and restore are user-visible latency paths. Before adding synchronous work there, ask:

- Is this per process, per VM, per request, or per byte?
- Does it touch disk, network, memory, or CPU?
- Can it be moved to configuration time or a background task?

### Avoid unbounded fan-out

Background tasks, accept loops, and guest bridges must have clear ownership and shutdown behavior.
Where loops are intentionally unbounded servers, keep the per-connection work bounded and isolated.

## Developer Experience

### Names carry domain meaning

Prefer names that match the architecture: `backend`, `work_dir`, `api_sock`, `rootfs`, `initramfs`,
`mmds`, `vsock`, `snapshot`, `pool`, `jailer`, `profile`. Avoid abbreviations unless they are domain
terms (`VZ`, `MMDS`, `UDS`, `FFI`).

### Say why, not only what

Comments should explain non-obvious constraints: VZ limitations, Firecracker compatibility choices,
sandbox permissions, entitlement requirements, FD ownership, EOF/half-close behavior, and guest
kernel quirks.

### Keep functions and scripts shaped for review

Push branching up and mechanics down. In scripts, use small helpers for artifact discovery, rootfs
cloning, cleanup, and repeated curl/API calls. In Rust/Swift, keep boundary-heavy functions short
enough that resource lifetime and error handling fit in one mental model.

### Prefer stable tooling

CI and local commands should be quiet on stable toolchains. Avoid nightly-only rustfmt options and
unnecessary toolchain-specific behavior.

## Review Framework

Use this checklist for every non-trivial change:

1. **Safety**
   - Are all trust/runtime boundaries explicit?
   - Does isolation fail closed?
   - Are resource lifetimes and cleanup clear?
   - Could any read/write pair deadlock or wait forever?

2. **Compatibility**
   - Does this preserve Firecracker wire shapes and state semantics?
   - Are documented deviations still accurate?
   - Does the Go SDK harness cover the changed behavior?

3. **Operability**
   - Does each failure mode point to the next clue?
   - Are logs, sockets, and generated files under a predictable owner?
   - Are recipes reproducible and self-cleaning?

4. **Performance**
   - Is new work on a boot/restore/data path justified?
   - Is fan-out bounded or intentionally server-like?

5. **Developer Experience**
   - Is the code named in the domain's language?
   - Do comments explain why where the code is surprising?
   - Do stable fmt, clippy, tests, compat, and relevant e2e pass?
