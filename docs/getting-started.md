# Getting started

This guide installs or builds Hephaestus and boots a Linux VM on a supported
Mac. The example is for development and evaluation, not untrusted workloads or
production multi-tenancy.

## Runtime requirements

- Apple Silicon Mac
- macOS 26 (Tahoe) or later
- An arm64 Linux kernel, init filesystem, and root filesystem for direct
  Virtualization.framework commands

## Install release binaries

Choose a version from [GitHub Releases](https://github.com/hephaestus-vm/hephaestus/releases),
download its Apple Silicon archive and checksum, and verify before extracting:

```console
$ VERSION=v0.4.0-alpha.1
$ ARCHIVE="hephaestus-${VERSION}-aarch64-apple-darwin"
$ curl --proto '=https' --tlsv1.2 -fLO \
    "https://github.com/hephaestus-vm/hephaestus/releases/download/${VERSION}/${ARCHIVE}.tar.gz"
$ curl --proto '=https' --tlsv1.2 -fLO \
    "https://github.com/hephaestus-vm/hephaestus/releases/download/${VERSION}/${ARCHIVE}.tar.gz.sha256"
$ shasum -a 256 -c "${ARCHIVE}.tar.gz.sha256"
$ tar -xzf "${ARCHIVE}.tar.gz"
```

For a per-user installation, copy the executables to `~/.local/bin`:

```console
$ mkdir -p "$HOME/.local/bin"
$ /usr/bin/install -m 0755 "$ARCHIVE"/hephaestus{,-firecracker,-jailer} \
    "$HOME/.local/bin/"
$ "$HOME/.local/bin/hephaestus" --version
```

If `~/.local/bin` is not already on `PATH`, add this to `~/.zprofile` and open a
new terminal:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

For an administrator-managed, system-wide installation, stage and verify the
archive as an ordinary user, then elevate only the final copy:

```console
$ sudo /usr/bin/install -m 0755 \
    "$ARCHIVE"/hephaestus{,-firecracker,-jailer} /usr/local/bin/
```

Do not copy unmanaged files into `/opt/homebrew`; that prefix belongs to
Homebrew. Verify the embedded signatures after installation:

```console
$ for binary in hephaestus hephaestus-firecracker hephaestus-jailer; do
    codesign --verify --strict "$(command -v "$binary")"
  done
```

A browser download may carry a quarantine attribute. Inspect and verify the
archive first. If Gatekeeper then blocks an ad-hoc-signed binary, remove the
attribute only from the verified installed files with
`xattr -d com.apple.quarantine <path>`.

## Source-build requirements

A source build additionally requires:

- Xcode 26 selected by `xcode-select`
- Rust 1.96 or the toolchain selected by `rust-toolchain.toml`
- [`just`](https://github.com/casey/just)
- [`apple/container`](https://github.com/apple/container)

Install the command-line prerequisites with Homebrew if needed:

```console
$ brew install rustup just container
$ rustup-init
$ xcode-select -p
```

Restart your shell after `rustup-init`. If several Xcode versions are installed,
select Xcode 26 before building.

## Populate guest artifacts

The repository's VM recipes reuse the kernel and filesystem artifacts cached by
`apple/container`:

```console
$ container system start
$ container run --rm docker.io/library/alpine:3.20 echo ready
```

Confirm that Hephaestus can discover them:

```console
$ just artifacts
kernel: .../vmlinux-...
initfs: .../snapshot
rootfs: .../snapshot
```

Artifact discovery fails with a next-step message if the cache is empty. The
layout belongs to `apple/container` and may change independently; the recipes
are the supported discovery path.

## Build and run

```console
$ git clone https://github.com/hephaestus-vm/hephaestus
$ cd hephaestus
$ just hello
```

`just hello` performs the following steps:

1. Builds the Rust workspace and Swift bridge.
2. Ad-hoc signs the binary with `com.apple.security.virtualization`.
3. Clones the cached root filesystem using APFS copy-on-write.
4. Boots a VM and runs `/bin/echo hello-from-hephaestus`.
5. Stops the VM when the command exits.

Expected guest output includes:

```text
hello-from-hephaestus
```

`just hello` is a source-tree convenience that discovers cached artifacts,
clones writable filesystems, and invokes the `hephaestus` binary. The equivalent
public CLI shape is:

```console
$ ./build/cargo_target/debug/hephaestus run \
    --id example \
    --kernel /absolute/path/to/vmlinux \
    --initfs /absolute/path/to/initfs.ext4 \
    --rootfs /absolute/path/to/writable-rootfs.ext4 \
    -- /bin/echo hello-from-hephaestus
```

Do not pass a shared writable cache image directly; clone the initfs and rootfs
first. Run `hephaestus run --help` for all CPU, memory, network, address, working
directory, and terminal options.

Try the diagnostic and interactive recipes next:

```console
$ just shell
$ just sh
$ just network-check
```

Use `exit` or Control-D to leave the interactive shell.

## Build outputs

A debug workspace build places the binaries under:

```text
build/cargo_target/debug/
├── hephaestus
├── hephaestus-firecracker
└── hephaestus-jailer
```

For an optimized build:

```console
$ cargo build --workspace --release
```

Release binaries are under `build/cargo_target/release/`. The build invokes
`scripts/link-and-sign.sh`; running an unsigned copy will fail when it attempts
to create a VM.

Pre-built artifacts follow the same layout and can be installed using the
verified release procedure above.

## Verify the installation

These checks do not boot a guest:

```console
$ ./build/cargo_target/debug/hephaestus --help
$ ./build/cargo_target/debug/hephaestus --version
$ ./build/cargo_target/debug/hephaestus ping
$ just verify-signing
$ just test
```

## Next steps

- [Use the CLI](guides/cli.md)
- [Use the Firecracker API](guides/firecracker-api.md)
- [Understand guest image inputs](guides/guest-images.md)
- [Run the test suites](development/testing.md)

## Troubleshooting

### No guest artifacts found

Run `container system start` and start at least one Linux container as shown
above, then retry `just artifacts`.

### Virtualization entitlement missing

Run `just verify-signing`. Ordinary builds should use the base
`hephaestus.entitlements` file automatically. Restricted vmnet networking has a
separate signing path; see [Privileged features](development/privileged-features.md).

### Swift or Xcode build errors

Confirm `xcode-select -p` points to Xcode 26 or later. The Swift package used by
the bridge requires the matching toolchain.

### VM recipes mutate or lock a root filesystem

Use the repository recipes rather than passing the cached
`apple/container` filesystem directly. Recipes clone writable filesystems before
booting them.
