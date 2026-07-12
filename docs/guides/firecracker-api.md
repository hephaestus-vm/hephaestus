# Firecracker API guide

`hephaestus-firecracker` is an HTTP/1.1 server on a UNIX socket. One process
owns at most one VM, matching Firecracker's process model.

This guide assumes `cargo build --workspace` has completed and you have an
arm64 Linux kernel and ext4 root filesystem. Source-tree users can run
`just artifacts` to locate development artifacts. Do not give the daemon a
writable cache image directly; clone it first.

Examples assume `hephaestus-firecracker` is on `PATH`. For a source build, use
`build/cargo_target/debug/hephaestus-firecracker` instead.

## Start the API process

```console
$ hephaestus-firecracker \
    --api-sock /tmp/hephaestus.sock \
    --id example
```

Use a separate process and socket for every VM.

## Configure a VM

In another terminal:

```console
$ sock=/tmp/hephaestus.sock
$ kernel=/absolute/path/to/vmlinux
$ rootfs=/absolute/path/to/rootfs.ext4

$ curl --unix-socket "$sock" -X PUT http://localhost/machine-config \
    -H 'Content-Type: application/json' \
    -d '{"vcpu_count":2,"mem_size_mib":512}'

$ curl --unix-socket "$sock" -X PUT http://localhost/boot-source \
    -H 'Content-Type: application/json' \
    -d "{\"kernel_image_path\":\"$kernel\",\"boot_args\":\"console=hvc0 reboot=k panic=1 pci=off\"}"

$ curl --unix-socket "$sock" -X PUT http://localhost/drives/rootfs \
    -H 'Content-Type: application/json' \
    -d "{\"drive_id\":\"rootfs\",\"path_on_host\":\"$rootfs\",\"is_root_device\":true,\"is_read_only\":false}"

$ curl --unix-socket "$sock" -X PUT http://localhost/actions \
    -H 'Content-Type: application/json' \
    -d '{"action_type":"InstanceStart"}'
```

Successful `PUT` requests return HTTP 204. Query instance state with:

```console
$ curl --unix-socket "$sock" http://localhost/
```

The guest must contain a valid init process and appropriate virtio drivers. Boot arguments are image-specific; use the arguments known to work with your image.

## Use Firecracker clients

The compatibility harness drives the server through the generated client in
`firecracker-go-sdk`:

```console
$ just fc-compat-config  # no VM boot; safe for CI
$ just fc-compat         # boots a VM using local artifacts
```

The same socket can be supplied to clients such as the Firecracker Go SDK and
`firectl`. A client must not assume support for every upstream device or field; consult [Firecracker compatibility](../firecracker-compatibility.md).

## Add networking

Before `InstanceStart`, configure a NIC:

```console
$ curl --unix-socket "$sock" -X PUT \
    http://localhost/network-interfaces/eth0 \
    -H 'Content-Type: application/json' \
    -d '{"iface_id":"eth0","guest_mac":"06:00:ac:10:00:02"}'
```

Hephaestus attaches VZ NAT. `host_dev_name` and rate limiters are accepted but
not implemented. The guest must run DHCP or configure its own address. See
[Networking and MMDS](networking.md).

## Snapshots and warm pools

The API supports pause/resume and full snapshot create/load. A process can also start with `--pool-dir <path>` and attempt a matching warm-pool restore on `InstanceStart`. See [Snapshots](snapshots.md) and [Warm pools](warm-pools.md).

## Errors

Hephaestus preserves Firecracker-style JSON error bodies where possible:

```json
{"fault_message":"..."}
```

State transitions are enforced. Configure boot resources and construction-time
devices before `InstanceStart`; use a new process for another VM.
