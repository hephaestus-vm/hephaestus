#!/bin/sh
set -eu

INSTALLER=${INSTALLER:-./install.sh}
tmpdir=$(mktemp -d "${TMPDIR:-/tmp}/hephaestus-installer-test.XXXXXX")
cleanup() {
    rm -rf "$tmpdir"
}
trap cleanup EXIT HUP INT TERM

expect_failure() {
    expected=$1
    shift
    if "$@" >"$tmpdir/stdout" 2>"$tmpdir/stderr"; then
        echo "expected command to fail: $*" >&2
        exit 1
    fi
    if ! grep -Fq -- "$expected" "$tmpdir/stderr"; then
        echo "expected error containing: $expected" >&2
        cat "$tmpdir/stderr" >&2
        exit 1
    fi
}

sh -n "$INSTALLER"
"$INSTALLER" --help | grep -Fq -- '--version VERSION'
expect_failure '--version is required' "$INSTALLER"
expect_failure 'invalid release version' "$INSTALLER" --version main
expect_failure 'invalid release version' "$INSTALLER" --version v0.4.0-alpha.1/other
expect_failure 'installation directory must be an absolute path' \
    "$INSTALLER" --version v0.4.0-alpha.1 --prefix relative
expect_failure 'choose only one destination' \
    "$INSTALLER" --version v0.4.0-alpha.1 --system --prefix /tmp/bin
expect_failure 'unknown option' "$INSTALLER" --wat

echo "Installer interface tests OK"
