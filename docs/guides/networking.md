# Networking and MMDS

Hephaestus has a base NAT path that works with ad-hoc signing and an
experimental shared-vmnet path that requires a profile-authorized build.

## NAT networking

A configured HTTP API network interface becomes a
`VZVirtioNetworkDeviceConfiguration` backed by
`VZNATNetworkDeviceAttachment`. The NAT network is normally
`192.168.64.0/24`, with the host-side gateway at `.1`.

Hephaestus honors `guest_mac`. Virtualization.framework manages the host side,
so Firecracker's `host_dev_name` has no equivalent. Receive and transmit rate
limiters are accepted but ignored because VZ exposes no corresponding knob.

As with Firecracker, attaching a device does not configure the guest's L3
network. The guest must use DHCP, a kernel `ip=` argument, cloud-init, or
another network manager. Enable NAT on the process-oriented CLI path with:

```console
$ hephaestus run \
    --id network-test \
    --kernel "$KERNEL" \
    --initfs "$INITFS" \
    --rootfs "$ROOTFS" \
    --network \
    -- /bin/sh -c 'ip addr; ip route'
```

When working from the repository, `just network-check` wraps this workflow and
adds an outbound HTTP request. Maintainers can use `just fc-compat-net-e2e` to
validate the Firecracker API path.

## Cross-VM isolation

Whether two VMs on one host can reach each other is measured, not assumed.
`just net-isolation-check` boots two NAT VMs and asserts that neither TCP
nor ICMP crosses between them (with a gateway control proving the probe
works); `just vmnet-isolation-check` does the same for two vmnet daemons
and additionally asserts they occupy distinct `/24` subnets. Both measure
`isolated` on macOS 26.5.2. VZ NAT's behavior is undocumented by Apple, so
the NAT smoke exists to catch an OS update changing it. A multi-VM shared
vmnet segment is not constructible: the network object must be created in
the attaching process, and Hephaestus runs one process per VM.

## Per-VM vmnet networking

On macOS 26 or later, the Firecracker daemon can replace VZ's opaque NAT
attachment with a `VZVmnetNetworkDeviceAttachment` backed by this VM's own
process-owned network:

```console
$ just sign-vmnet
$ build/HephaestusFirecracker.app/Contents/MacOS/hephaestus-firecracker \
    --network-backend vmnet \
    --host-mmds \
    --api-sock /tmp/hephaestus-firecracker.socket
```

Each daemon creates its own network object, so every VM gets its own `/24`
subnet — that separation is the cross-VM isolation guarantee above. The
daemon prints the subnet to stderr at boot
(`vmnet network subnet=… mask=…`) and records it in a `.vmnet-subnet`
sidecar next to the serial log, so relaunches and snapshot restores in the
same work dir land the guest back on the subnet its DHCP lease came from.
Delete the sidecar to reallocate; a corrupt or unsatisfiable pin fails
closed with the same remedy. Snapshots moved to a *different* work dir get
a fresh subnet and the guest must re-DHCP.

The host-side vmnet packet interface — an extra host NIC on the VM's
segment — exists only under `--host-mmds`, and it sets vmnet's isolation
option so different daemons' packet interfaces cannot reach each other.

The app bundle embeds the provisioning profile that authorizes
`com.apple.vm.networking`; signing the standalone Mach-O is insufficient. Run
`just probe-vmnet` first to validate both AMFI authorization and construction of
the vmnet attachment (an unauthorized daemon fails with an error naming that
probe). NAT remains the default and release builds do not claim the restricted
entitlement. Under the jailer, pass
`--network-backend vmnet --firecracker-binary <app-bundle binary>`.

## Metadata service

The Firecracker MMDS endpoints store and return arbitrary JSON. For direct-VZ
VMs, Hephaestus also serves the current document over virtio-vsock port `16992`.

The bundled guest agent starts a link-local shim by default. It:

1. Configures `169.254.169.254/32` on guest loopback.
2. Listens on `169.254.169.254:80` inside the guest.
3. Forwards requests to host vsock port `16992`.

Disable automatic startup with `HEPHAESTUS_MMDS_SHIM=0` or the kernel argument
`hephaestus.mmds=off`. A custom image can invoke
`hephaestus-agent mmds-shim` itself.

Configured metadata is intentionally visible to guest processes. Do not place a secret in MMDS unless every process in the guest may read it.

## Arbitrary stock images

The agent shim provides the familiar link-local URL only to images that include the helper. Transparent MMDS for arbitrary images needs a network attachment where the host can answer on the guest's L2. VZ NAT is a black box and cannot be used for this.

With `--host-mmds --network-backend vmnet`, Hephaestus claims
`169.254.169.254` on the VM's virtual Ethernet segment, answers ARP, and serves
MMDS using a small user-space TCP/HTTP responder. It does not add host interface
aliases, routes, packet-filter rules, or require root. A stock guest with its
NIC configured can therefore use the standard URL directly. Run
`just fc-compat-vmnet-e2e` for the real-VM DHCP and HTTP smoke. See
[Privileged features](../development/privileged-features.md) for signing setup.

## Vsock

`PUT /vsock` creates a Firecracker-compatible host UDS bridge after boot. A host client connects to the configured socket and writes:

```text
CONNECT <guest-port>\n
```

The remaining byte stream is bridged to the VZ virtio socket device. VZ assigns the real CID, so a configured Firecracker `guest_cid` is retained only for wire compatibility. Port 1234 is reserved by convention for `hephaestus-agent`, and port 16992 is the MMDS service.

Run `just fc-compat-vsock-e2e` for the real-VM vsock and MMDS smoke test.
