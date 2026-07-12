# Compatibility testing

`compat/firectl-harness` drives Hephaestus through the swagger-generated client
from `firecracker-go-sdk`. It catches wire-shape, required-field, response, and
lifecycle drift that hand-written `curl` requests may miss.

## CI-safe run

```console
$ just fc-compat-config
```

The script starts `hephaestus-firecracker` with dummy kernel/rootfs paths and
runs the harness with boot disabled. This verifies the full configuration
sequence without constructing a VZ VM.

The restrictive counterpart is:

```console
$ just fc-compat-sandbox-config
```

It additionally proves the generated sandbox denies a path outside its
allowlist.

## Real-VM runs

`just fc-compat` performs the cold-boot sequence. Dedicated recipes cover NAT,
vsock/MMDS, snapshots, and warm pools. The
[testing guide](testing.md) lists the suite.

## Updating Firecracker compatibility

The upstream reference source lives under `vendor/firecracker/`. Wire types in
`hephaestus-fc-api` carry upstream pointers so synchronization can be reviewed
mechanically.

When adopting a new Firecracker API version:

1. Update the vendored reference according to its README.
2. Diff and synchronize the relevant wire types and routes.
3. Decide the semantic mapping for every added field or operation.
4. Extend the Go SDK harness.
5. Run config-only and affected real-VM tests.
6. Update [Firecracker compatibility](../firecracker-compatibility.md).
7. Only then update the version reported by `GET /version`.

A successful deserialization is not enough: state restrictions, status codes,
error bodies, and accepted-but-ignored fields are part of the compatibility
contract.
