#!/usr/bin/env python3
"""Build vsock E2E payloads and drive its guest-agent assertions."""

from __future__ import annotations

import argparse
import json
import socket
import struct
import sys
import threading
import time
from typing import Any

JsonObject = dict[str, Any]
ECHO_PORT = 2345
ECHO_TOKEN = b"hephaestus-generic-vsock-echo"
INSTANCE_ID = "i-hephaestus-vsock-e2e"


def logger_config(path: str) -> JsonObject:
    return {
        "log_path": path,
        "level": "Debug",
        "show_level": True,
        "show_log_origin": True,
    }


def metrics_config(path: str) -> JsonObject:
    return {"metrics_path": path}


def vsock_config(path: str) -> JsonObject:
    return {"guest_cid": 3, "uds_path": path}


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


def guest_command() -> bytes:
    return (
        f"__hephaestus_test_vsock_suite {INSTANCE_ID} "
        f"{ECHO_PORT} {ECHO_TOKEN.decode()}"
    ).encode()


def connect_with_retry(path: str, port: int) -> socket.socket:
    last: Exception | None = None
    for _ in range(160):
        connection: socket.socket | None = None
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
            if connection is not None:
                connection.close()
            last = error
            time.sleep(0.25)
    raise RuntimeError(f"could not connect to guest port {port}: {last}")


def echo_client(path: str, result: list[Exception | None]) -> None:
    last: Exception | None = None
    for _ in range(80):
        try:
            connection = connect_with_retry(path, ECHO_PORT)
            # The guest halts immediately after echoing. Bound every receive so
            # a short response cannot wedge interpreter shutdown.
            connection.settimeout(10)
            connection.sendall(ECHO_TOKEN)
            data = b""
            while len(data) < len(ECHO_TOKEN):
                chunk = connection.recv(len(ECHO_TOKEN) - len(data))
                if not chunk:
                    raise RuntimeError("short echo read")
                data += chunk
            if data.startswith(b"ERR "):
                raise RuntimeError(data + connection.recv(256))
            if data != ECHO_TOKEN:
                raise RuntimeError(f"echo mismatch: {data!r}")
            result.append(None)
            return
        except Exception as error:
            last = error
            time.sleep(0.25)
    result.append(last)


def check_guest(path: str) -> None:
    command_bytes = guest_command()
    last: Exception | None = None
    for _ in range(80):
        try:
            command = connect_with_retry(path, 1234)
            command.sendall(struct.pack("<I", len(command_bytes)) + command_bytes)
            echo_result: list[Exception | None] = []
            # A blocked attempt must not keep the interpreter alive. The join
            # timeout and echo_result determine the test verdict.
            echo_thread = threading.Thread(
                target=echo_client,
                args=(path, echo_result),
                daemon=True,
            )
            echo_thread.start()

            data = b""
            while len(data) < 4:
                chunk = command.recv(4 - len(data))
                if not chunk:
                    raise RuntimeError("short exit-code read")
                data += chunk
            if data.startswith(b"ERR "):
                raise RuntimeError(data + command.recv(256))
            code = struct.unpack("<i", data)[0]
            echo_thread.join(timeout=10)
            if echo_thread.is_alive():
                raise RuntimeError("generic echo test timed out")
            if echo_result and echo_result[0] is not None:
                raise echo_result[0]
            if code != 0:
                raise RuntimeError(f"guest vsock suite exited {code}")
            print("guest MMDS vsock test exited 0")
            print("guest MMDS link-local shim test exited 0")
            print("generic guest-port vsock echo test exited 0")
            return
        except Exception as error:
            print(
                f"vsock-e2e attempt failed: {type(error).__name__}: {error!r}",
                file=sys.stderr,
            )
            last = error
            time.sleep(0.25)
    raise RuntimeError(f"could not complete vsock e2e: {last}")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    logger = subparsers.add_parser("logger-config")
    logger.add_argument("path")

    metrics = subparsers.add_parser("metrics-config")
    metrics.add_argument("path")

    vsock = subparsers.add_parser("vsock-config")
    vsock.add_argument("path")

    boot = subparsers.add_parser("boot-config")
    boot.add_argument("kernel")
    boot.add_argument("initrd")

    drive = subparsers.add_parser("drive-config")
    drive.add_argument("rootfs")

    guest = subparsers.add_parser("check-guest")
    guest.add_argument("vsock")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.command == "logger-config":
        value = logger_config(args.path)
    elif args.command == "metrics-config":
        value = metrics_config(args.path)
    elif args.command == "vsock-config":
        value = vsock_config(args.path)
    elif args.command == "boot-config":
        value = boot_config(args.kernel, args.initrd)
    elif args.command == "drive-config":
        value = drive_config(args.rootfs)
    elif args.command == "check-guest":
        check_guest(args.vsock)
        return 0
    else:
        raise AssertionError(f"unknown command: {args.command}")
    print(json.dumps(value))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
