#!/usr/bin/env bash
#
# Build the hephaestus-agent guest binary (aarch64-linux-musl) and package
# it as a minimal gzipped cpio initramfs suitable for passing to
# VZLinuxBootLoader.initialRamdiskURL.
#
# Output: build/agent.cpio.gz
#
# Requires:
#   - rustup with the aarch64-unknown-linux-musl target
#     (`rustup target add aarch64-unknown-linux-musl`). rustup ships
#     `rust-lld` + self-contained musl crt for this target, so no separate
#     cross-toolchain (zig, musl-cross, Docker) is needed.
#   - cpio + gzip (macOS base system)

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
agent_dir="$repo_root/guest/hephaestus-agent"
out_dir="$repo_root/build"
mkdir -p "$out_dir"

# Homebrew's cargo doesn't know about rustup-installed targets, so push
# $HOME/.cargo/bin to the front of PATH to pick up the rustup proxy.
if [[ -x "$HOME/.cargo/bin/cargo" ]]; then
    export PATH="$HOME/.cargo/bin:$PATH"
fi

echo "[build-agent] cross-compiling guest/hephaestus-agent for aarch64-linux-musl…"
(cd "$agent_dir" && cargo build --release)

binary="$agent_dir/target/aarch64-unknown-linux-musl/release/hephaestus-agent"
if [[ ! -f "$binary" ]]; then
    echo "error: expected binary at $binary but it's missing" >&2
    exit 1
fi

# Stage a tiny root containing just /init so the kernel's initramfs loader
# finds PID 1 at the conventional location. Nothing else needs to be in the
# cpio — our agent mounts everything it needs.
staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT
install -m 0755 "$binary" "$staging/init"

# Produce a gzipped `newc`-format cpio archive; that's what Linux kernels
# expect for early initramfs. We prefix a `cd` so paths end up relative.
(
    cd "$staging"
    # -o: output, -H newc: Linux kernel format. Feeding file list via find.
    find . -print | cpio -o -H newc --quiet
) | gzip -9 > "$out_dir/agent.cpio.gz"

size=$(stat -f %z "$out_dir/agent.cpio.gz")
echo "[build-agent] wrote $out_dir/agent.cpio.gz (${size} bytes)"
