# Guest agent

`guest/hephaestus-agent` is a small statically linked arm64 Linux program used by
the direct-VZ execution and agent warm-pool paths.

## Boot lifecycle

When started as PID 1, the agent:

1. Mounts the required pseudo-filesystems and root filesystem.
2. Enters the root filesystem.
3. Starts the optional link-local MMDS shim.
4. Listens on virtio-vsock port 1234.
5. Receives a length-prefixed shell command from the host.
6. Executes it through `/bin/sh -c` with console streams attached.
7. Returns the exit status and powers off the guest.

The warm-pool snapshot is taken while the agent is waiting for a command. After
restore, the host can send work without repeating the Linux boot sequence.

## Console streams

The direct execution path uses virtio consoles for guest output. Standard error
uses a separate console when available. Snapshot-based workflows use URL-backed
serial attachments because a restored VZ configuration cannot attach the
original host process's file descriptors.

## Metadata shim

The agent can expose Firecracker's link-local metadata URL inside a controlled
guest. `hephaestus-agent mmds-shim` listens on `169.254.169.254:80` and forwards
HTTP to host vsock port 16992. PID 1 mode starts it automatically unless
`HEPHAESTUS_MMDS_SHIM=0` or `hephaestus.mmds=off` is set.

This is a transport shim, not an authorization boundary. All guest processes
that can reach the URL can read configured metadata.

## Build

```console
$ rustup target add aarch64-unknown-linux-musl
$ just build-agent
```

The recipe cross-compiles the agent and packages `build/agent.cpio.gz`. Rebuild
it after changing guest-side code.

## Protocol ownership

Ports 1234 and 16992 are reserved by convention for command delivery and MMDS.
Protocol reads must have bounded waits where a stalled peer could block VM
startup or teardown. Host and guest changes to framing, stream ordering, or
half-close behavior must land together and include a real-VM e2e test.
