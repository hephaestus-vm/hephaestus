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


def network_config() -> JsonObject:
    return {
        "iface_id": "eth0",
        "host_dev_name": "tap0",
        "guest_mac": "AA:FC:00:00:00:01",
    }


def boot_config(kernel: str, initrd: str) -> JsonObject:
    return {
        "kernel_image_path": kernel,
        "initrd_path": initrd,
        "boot_args": "console=hvc0 rdinit=/init quiet loglevel=3",
    }


def drive_config(rootfs: str) -> JsonObject:
    return {
        "drive_id": "rootfs",
        "path_on_host": rootfs,
        "is_root_device": True,
        "is_read_only": False,
    }


def guest_command(test_mmds: bool) -> bytes:
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


def check_guest(path: str, test_mmds: bool) -> None:
    command_bytes = guest_command(test_mmds)
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
                assertion = "MMDS fetch" if test_mmds else "network device check"
                raise RuntimeError(f"guest {assertion} failed (agent exit {code})")
            if test_mmds:
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

    subparsers.add_parser("network-config")

    boot = subparsers.add_parser("boot-config")
    boot.add_argument("kernel")
    boot.add_argument("initrd")

    drive = subparsers.add_parser("drive-config")
    drive.add_argument("rootfs")

    guest = subparsers.add_parser("check-guest")
    guest.add_argument("vsock")
    guest.add_argument("--mmds", action="store_true")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.command == "vsock-config":
        print(json.dumps(vsock_config(args.path)))
    elif args.command == "network-config":
        print(json.dumps(network_config()))
    elif args.command == "boot-config":
        print(json.dumps(boot_config(args.kernel, args.initrd)))
    elif args.command == "drive-config":
        print(json.dumps(drive_config(args.rootfs)))
    elif args.command == "check-guest":
        check_guest(args.vsock, args.mmds)
    else:
        raise AssertionError(f"unknown command: {args.command}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
