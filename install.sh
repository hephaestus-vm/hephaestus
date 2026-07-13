#!/bin/sh
# Install a checksum-verified Hephaestus release on Apple Silicon macOS.
set -eu

PROGRAM=${0##*/}
REPOSITORY=https://github.com/hephaestus-vm/hephaestus
VERSION=
INSTALL_DIR=${HOME:?HOME must be set}/.local/bin
SYSTEM_INSTALL=0
DRY_RUN=0
DESTINATION_SET=0

usage() {
    cat <<EOF
Usage: $PROGRAM --version VERSION [--prefix DIRECTORY | --system] [--dry-run]

Options:
  --version VERSION  Release tag to install, for example v0.4.0-alpha.1
  --prefix DIRECTORY Install into DIRECTORY (default: \$HOME/.local/bin)
  --system           Install into /usr/local/bin, elevating only final copies
  --dry-run          Download and verify without installing
  -h, --help         Show this help
EOF
}

fail() {
    echo "$PROGRAM: $*" >&2
    exit 1
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --version)
            [ "$#" -ge 2 ] || fail "--version requires a value"
            VERSION=$2
            shift 2
            ;;
        --prefix)
            [ "$#" -ge 2 ] || fail "--prefix requires a value"
            [ "$DESTINATION_SET" -eq 0 ] || fail "choose only one destination"
            INSTALL_DIR=$2
            DESTINATION_SET=1
            shift 2
            ;;
        --system)
            [ "$DESTINATION_SET" -eq 0 ] || fail "choose only one destination"
            INSTALL_DIR=/usr/local/bin
            SYSTEM_INSTALL=1
            DESTINATION_SET=1
            shift
            ;;
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            fail "unknown option: $1"
            ;;
    esac
done

[ -n "$VERSION" ] || fail "--version is required"
printf '%s\n' "$VERSION" \
    | /usr/bin/grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z][0-9A-Za-z.-]*)?$' \
    || fail "invalid release version: $VERSION"
case "$INSTALL_DIR" in
    /*) ;;
    *) fail "installation directory must be an absolute path" ;;
esac

[ "$(uname -s)" = Darwin ] || fail "Hephaestus release binaries require macOS"
[ "$(uname -m)" = arm64 ] || fail "Hephaestus release binaries require native Apple Silicon"
[ "$(id -u)" -ne 0 ] || fail "run as your user, not with sudo; use --system for a system installation"

for command in curl shasum tar codesign grep; do
    command -v "$command" >/dev/null 2>&1 || fail "required command not found: $command"
done
[ -x /usr/bin/install ] || fail "required command not found: /usr/bin/install"
if [ "$SYSTEM_INSTALL" -eq 1 ]; then
    command -v sudo >/dev/null 2>&1 || fail "sudo is required with --system"
fi

triple=aarch64-apple-darwin
archive="hephaestus-${VERSION}-${triple}"
tarball="${archive}.tar.gz"
checksum="${tarball}.sha256"
base_url="${REPOSITORY}/releases/download/${VERSION}"

tmpdir=$(mktemp -d "${TMPDIR:-/tmp}/hephaestus-install.XXXXXX")
cleanup() {
    rm -rf "$tmpdir"
}
trap cleanup EXIT HUP INT TERM
umask 077

echo "Downloading $tarball"
curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error \
    --output "$tmpdir/$tarball" "$base_url/$tarball"
curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error \
    --output "$tmpdir/$checksum" "$base_url/$checksum"

(
    cd "$tmpdir"
    shasum -a 256 -c "$checksum"
)

tar -tzf "$tmpdir/$tarball" > "$tmpdir/entries"
if grep -Eq '(^/|(^|/)\.\.(/|$))' "$tmpdir/entries"; then
    fail "release archive contains an unsafe path"
fi
tar -xzf "$tmpdir/$tarball" -C "$tmpdir"

for binary in hephaestus hephaestus-firecracker hephaestus-jailer; do
    path="$tmpdir/$archive/$binary"
    [ -f "$path" ] || fail "release archive is missing $binary"
    codesign --verify --strict "$path"
done
for binary in hephaestus hephaestus-firecracker; do
    path="$tmpdir/$archive/$binary"
    codesign -d --entitlements - "$path" 2>&1 \
        | grep -q com.apple.security.virtualization \
        || fail "$binary is missing the virtualization entitlement"
done

echo "Verified checksum, signatures, and virtualization entitlements"
if [ "$DRY_RUN" -eq 1 ]; then
    echo "Dry run complete; would install into $INSTALL_DIR"
    exit 0
fi

if [ "$SYSTEM_INSTALL" -eq 1 ]; then
    echo "Installing into $INSTALL_DIR; sudo is used only for these final copies"
    sudo /bin/mkdir -p "$INSTALL_DIR"
    for binary in hephaestus hephaestus-firecracker hephaestus-jailer; do
        source="$tmpdir/$archive/$binary"
        staging="$INSTALL_DIR/.${binary}.install.$$"
        sudo /usr/bin/install -m 0755 "$source" "$staging"
        sudo /bin/mv -f "$staging" "$INSTALL_DIR/$binary"
    done
else
    /bin/mkdir -p "$INSTALL_DIR"
    [ -w "$INSTALL_DIR" ] || fail "installation directory is not writable: $INSTALL_DIR"
    for binary in hephaestus hephaestus-firecracker hephaestus-jailer; do
        source="$tmpdir/$archive/$binary"
        staging="$INSTALL_DIR/.${binary}.install.$$"
        /usr/bin/install -m 0755 "$source" "$staging"
        /bin/mv -f "$staging" "$INSTALL_DIR/$binary"
    done
fi

for binary in hephaestus hephaestus-firecracker hephaestus-jailer; do
    codesign --verify --strict "$INSTALL_DIR/$binary"
done

echo "Installed Hephaestus $VERSION into $INSTALL_DIR"
case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) echo "Add $INSTALL_DIR to PATH before invoking the installed commands." ;;
esac
echo "Uninstall with: rm '$INSTALL_DIR/hephaestus' '$INSTALL_DIR/hephaestus-firecracker' '$INSTALL_DIR/hephaestus-jailer'"
