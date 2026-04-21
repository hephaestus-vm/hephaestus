#!/usr/bin/env bash
# Wrapper around `zig cc` that pins the cross-compile target triple. Used as
# rustc's linker for the guest agent (`guest/hephaestus-agent`). zig bundles
# lld + musl libc + headers, so no separate sysroot or cross-toolchain is
# needed.
exec zig cc -target aarch64-linux-musl "$@"
