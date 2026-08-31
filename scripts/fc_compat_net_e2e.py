#!/usr/bin/env python3
"""Build network E2E payloads and drive its guest-agent assertion."""

from __future__ import annotations

import argparse
import json
import socket
import struct
import sys
import time
from typing import Any

JsonObject = dict[str, Any]


def vsock_config(path: str) -> JsonObject:
    return {"guest_cid": 3, "uds_path": path}


def network_config(mac: str = "AA:FC:00:00:00:01") -> JsonObject:
    return {
        "iface_id": "eth0",
        "host_dev_name": "tap0",
        "guest_mac": mac,
    }


def boot_config(kernel: str, initrd: str, mmds_off: bool = False) -> JsonObject:
    boot_args = "console=hvc0 rdinit=/init quiet loglevel=3"
    if mmds_off:
        # Disable the guest agent's link-local MMDS shim: it answers
        # 169.254.169.254 too, and the no-mmds assertion must observe the
        # host responder's absence, not the shim's presence.
        boot_args += " hephaestus.mmds=off"
    return {
        "kernel_image_path": kernel,
        "initrd_path": initrd,
        "boot_args": boot_args,
    }


def drive_config(rootfs: str) -> JsonObject:
    return {
        "drive_id": "rootfs",
        "path_on_host": rootfs,
        "is_root_device": True,
        "is_read_only": False,
    }


def guest_command(test_mmds: bool, expect_no_mmds: bool = False) -> bytes:
    if expect_no_mmds:
        # The NIC and DHCP must work, but the metadata fetch must FAIL:
        # observable proof that no host packet interface answers on the
        # segment (the shim is disabled via hephaestus.mmds=off).
        return b'''set -e
iface="$(ls /sys/class/net 2>/dev/null | grep -v '^lo$' | head -1)"
test -n "$iface"
ip link set "$iface" up
udhcpc -i "$iface" -n -q
! wget -qO- -T 5 http://169.254.169.254/latest/meta-data/instance-id 2>/dev/null'''
    if not test_mmds:
        return b'test -n "$(ls /sys/class/net 2>/dev/null | grep -v \'^lo$\')"'
    return b'''set -e
iface="$(ls /sys/class/net 2>/dev/null | grep -v '^lo$' | head -1)"
test -n "$iface"
ip link set "$iface" up
udhcpc -i "$iface" -n -q
value="$(curl -fsS --max-time 10 http://169.254.169.254/latest/meta-data/instance-id 2>/dev/null || wget -qO- -T 10 http://169.254.169.254/latest/meta-data/instance-id)"
test "$value" = i-hephaestus-vmnet'''


def connect_with_retry(path: str, port: int) -> socket.socket:
    last: Exception | None = None
    for _ in range(160):
        try:
            connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            connection.connect(path)
            connection.sendall(f"CONNECT {port}\n".encode())
            connection.settimeout(0.05)
            try:
                data = connection.recv(4, socket.MSG_PEEK)
                if data.startswith(b"ERR "):
                    raise RuntimeError(connection.recv(256))
            except TimeoutError:
                pass
            finally:
                connection.settimeout(None)
            return connection
        except Exception as error:
            last = error
            time.sleep(0.25)
    raise RuntimeError(f"could not connect to guest port {port}: {last}")


def run_guest(path: str, command: str, no_wait: bool) -> int:
    """Run a command in the guest; return the agent's exit code.

    The agent serves exactly ONE command per boot and powers the VM off
    after replying. Two consequences shape this function: a probe VM that
    must stay alive gets a command ending in a long sleep, sent with
    `no_wait` (the exit code is never collected — process exit closes the
    socket, which is safe because the length-prefixed command was already
    delivered); and a command must NEVER be re-sent, because a retry after
    delivery would land on a powered-off VM at best and double-execute at
    worst. Only pre-delivery failures — connect errors and the bridge's
    explicit "ERR" refusal while the guest port is not yet listening — are
    retried, inside connect_with_retry and the ERR check below.
    """
    command_bytes = command.encode()
    for _ in range(40):
        connection = connect_with_retry(path, 1234)
        # Give a late bridge refusal a real chance to arrive before we
        # commit the command (connect_with_retry's own peek is 50ms).
        connection.settimeout(0.5)
        try:
            data = connection.recv(4, socket.MSG_PEEK)
            if data.startswith(b"ERR "):
                print(f"run-guest: bridge refused: {connection.recv(256)!r}", file=sys.stderr)
                time.sleep(0.25)
                continue
        except TimeoutError:
            pass
        connection.settimeout(90)
        connection.sendall(struct.pack("<I", len(command_bytes)) + command_bytes)
        if no_wait:
            return 0
        data = b""
        while len(data) < 4:
            chunk = connection.recv(4 - len(data))
            if not chunk:
                raise RuntimeError(
                    "connection closed before the exit code arrived; the command "
                    "was already delivered, so this is fatal (no resend)"
                )
            data += chunk
        if data.startswith(b"ERR "):
            raise RuntimeError(data + connection.recv(256))
        return struct.unpack("<i", data)[0]
    raise RuntimeError("guest port 1234 never accepted the command")


def check_guest(path: str, test_mmds: bool, expect_no_mmds: bool = False) -> None:
    command_bytes = guest_command(test_mmds, expect_no_mmds)
    last: Exception | None = None
    for _ in range(80):
        try:
            command = connect_with_retry(path, 1234)
            command.settimeout(30)
            command.sendall(struct.pack("<I", len(command_bytes)) + command_bytes)
            data = b""
            while len(data) < 4:
                chunk = command.recv(4 - len(data))
                if not chunk:
                    raise RuntimeError("short exit-code read")
                data += chunk
            if data.startswith(b"ERR "):
                raise RuntimeError(data + command.recv(256))
            code = struct.unpack("<i", data)[0]
            if code != 0:
                assertion = (
                    "no-MMDS check (metadata unexpectedly reachable?)"
                    if expect_no_mmds
                    else "MMDS fetch" if test_mmds else "network device check"
                )
                raise RuntimeError(f"guest {assertion} failed (agent exit {code})")
            if expect_no_mmds:
                print("guest networking works and the metadata service is absent")
            elif test_mmds:
                print("guest fetched transparent MMDS over vmnet")
            else:
                print("guest sees a non-loopback network device")
            return
        except Exception as error:
            print(f"net-e2e attempt failed: {type(error).__name__}: {error!r}", file=sys.stderr)
            last = error
            time.sleep(0.25)
    raise RuntimeError(f"could not complete net e2e: {last}")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    vsock = subparsers.add_parser("vsock-config")
    vsock.add_argument("path")

    network = subparsers.add_parser("network-config")
    network.add_argument("--mac", default="AA:FC:00:00:00:01")

    run = subparsers.add_parser("run-guest")
    run.add_argument("vsock")
    run.add_argument("guest_command")
    run.add_argument("--no-wait", action="store_true")

    boot = subparsers.add_parser("boot-config")
    boot.add_argument("kernel")
    boot.add_argument("initrd")
    boot.add_argument("--mmds-off", action="store_true")

    drive = subparsers.add_parser("drive-config")
    drive.add_argument("rootfs")

    guest = subparsers.add_parser("check-guest")
    guest.add_argument("vsock")
    guest.add_argument("--mmds", action="store_true")
    guest.add_argument("--expect-no-mmds", action="store_true")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.command == "vsock-config":
        print(json.dumps(vsock_config(args.path)))
    elif args.command == "network-config":
        print(json.dumps(network_config(args.mac)))
    elif args.command == "run-guest":
        print(run_guest(args.vsock, args.guest_command, args.no_wait))
    elif args.command == "boot-config":
        print(json.dumps(boot_config(args.kernel, args.initrd, args.mmds_off)))
    elif args.command == "drive-config":
        print(json.dumps(drive_config(args.rootfs)))
    elif args.command == "check-guest":
        check_guest(args.vsock, args.mmds, args.expect_no_mmds)
    else:
        raise AssertionError(f"unknown command: {args.command}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
