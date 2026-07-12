# Testing

Hephaestus separates CI-safe tests from local tests that boot a real VM or need
restricted host capabilities.

## Required pull-request checks

```console
$ cargo fmt --all -- --check
$ cargo clippy --workspace --all-targets -- -D warnings
$ cargo test --workspace
```

`just test` runs workspace tests plus VM-free bridge and rootfs checks. CI also builds the Swift bridge as part of the workspace build.

## Firecracker control-plane compatibility

These tests use dummy guest paths and do not construct a VM:

```console
$ just fc-compat-config
$ just fc-compat-sandbox-config
```

Both run in GitHub Actions. The second enters a restrictive generated sandbox
and verifies that an unrelated host path is denied.

## Real-VM smoke tests

The following require artifacts discoverable by `just artifacts` and are not
ordinary hosted-runner tests:

```console
$ just hello
$ just network-check
$ just fc-compat
$ just fc-compat-net-e2e
$ just fc-compat-vsock-e2e
$ just fc-compat-snapshot
$ just fc-compat-pool
$ just fc-compat-pool-stock
```

Restrictive sandbox variants exist for cold boot, vsock/MMDS, snapshots, and
both pool flavors:

```console
$ just fc-compat-sandbox
$ just fc-compat-sandbox-vsock-e2e
$ just fc-compat-sandbox-snapshot
$ just fc-compat-sandbox-pool
$ just fc-compat-sandbox-pool-stock
```

Run the narrowest relevant smoke while iterating and the complete affected
family before opening a pull request.

## Test expectations by area

| Changed area | Additional validation |
| :-- | :-- |
| Firecracker wire type or route | Config-only Go SDK compatibility |
| VM construction or device | Relevant real-VM e2e |
| Swift/Rust ABI | `just ping`, unit tests, and a real-VM path |
| Agent or vsock | `just build-agent` and vsock e2e |
| Snapshot or pool | Save/load or both pool-flavor tests |
| Sandbox or jailer | Restrictive config test and relevant sandbox e2e |
| Documentation command | Run the command on a supported clean environment |

## Timeouts and cleanup

VM, socket, vsock, and process tests must use bounded waits. Scripts should
install cleanup traps immediately after creating processes or mutable artifacts
and print retained log paths on failure. These are correctness requirements, not
only test hygiene.

For details of the SDK harness, see [Compatibility testing](compatibility-testing.md).
