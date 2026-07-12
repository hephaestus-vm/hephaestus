# Rust/Swift FFI

The bridge exposes Apple virtualization APIs to the Rust workspace through a C
ABI. Rust owns orchestration and API state; Swift owns framework objects and
asynchronous VZ calls.

## Build path

1. `src/hephaestus-bridge/build.rs` generates the C header with cbindgen.
2. The build script invokes `xcrun swift build` for arm64 macOS.
3. Cargo links the resulting static archive into the Rust binaries.
4. `scripts/link-and-sign.sh` signs executable outputs with the selected
   entitlement file and identity.

The generated header is:

```text
swift/HephaestusBridge/Sources/CHephaestusBridge/include/hephaestus_bridge.h
```

Swift entry points use `@_cdecl`; corresponding Rust declarations use
`unsafe extern "C"`. Symbol names, argument positions, widths, and ownership
must remain in lockstep.

## Ownership contract

Opaque handles represent long-lived Swift objects across the ABI. Calls use a
status value plus out-parameters rather than allowing Swift errors to cross C.
The Rust wrapper checks status immediately and is responsible for invoking the
matching release function exactly once.

Strings and arrays passed into Swift are borrowed only for the duration of the
call unless an entry point explicitly copies them. File descriptors returned to Rust have documented transfer or duplication semantics. Boundary changes must be reviewed for nullability, lifetime, thread, queue, and teardown behavior.

## Linking constraint

The Swift archive is linked with `-Wl,-force_load`. This is load-bearing:
without it, required Swift type metadata may not be registered and allocation
can fail at runtime. Binaries also carry the Swift runtime rpath required by
newer Xcode toolchains.

## Restore instrumentation

Restore functions fill an `HbRestoreTimings` structure with configuration,
construction, restore, and resume durations. The pool layer adds rootfs clone
time. See [Performance](../performance.md).

## Changing the boundary

When adding or changing an entry point:

1. Update the generated-header source and Swift implementation together.
2. Update every Rust declaration and wrapper call.
3. State pointer ownership and FD semantics in code comments.
4. Return errors through the shared status mechanism.
5. Run formatting, workspace tests, the FFI ping, and a relevant real-VM smoke.

The [engineering style](engineering-style.md) treats FFI as an explicit trust
and resource-lifetime boundary.
