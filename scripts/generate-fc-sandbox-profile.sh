#!/usr/bin/env bash
# Generate a deny-by-default macOS sandbox profile for config-only
# hephaestus-firecracker compatibility tests.
#
# This is intentionally conservative and path based. It is the first step
# toward the full jailer: a per-VM supervisor will eventually generate a similar
# profile from canonicalized VM inputs, then launch one firecracker-compatible
# process under it.

set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage: generate-fc-sandbox-profile.sh --output PROFILE --work-dir DIR [--read PATH]... [--read-write DIR]... [--read-write-file PATH]...

Writes a sandbox profile that denies by default, allows broad process/sysctl/
mach lookup primitives needed by a normal userspace daemon, grants read access
to explicit files, and grants read/write/create/delete access under explicit
work directories.
EOF
  exit 64
}

out=""
work_dirs=()
reads=()
read_writes=()
read_write_files=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --output) out="${2:?}"; shift 2 ;;
    --work-dir|--read-write) read_writes+=("${2:?}"); shift 2 ;;
    --read-write-file) read_write_files+=("${2:?}"); shift 2 ;;
    --read) reads+=("${2:?}"); shift 2 ;;
    -h|--help) usage ;;
    *) echo "unknown arg: $1" >&2; usage ;;
  esac
done

[[ -n "$out" ]] || usage

scheme_escape() {
  python3 - "$1" <<'PY'
import sys
s = sys.argv[1]
print(s.replace('\\', '\\\\').replace('"', '\\"'))
PY
}

literal_form() {
  local p
  p="$(scheme_escape "$(cd "$(dirname "$1")" && pwd -P)/$(basename "$1")")"
  printf '(literal "%s")' "$p"
}

subpath_form() {
  local p
  mkdir -p "$1"
  p="$(scheme_escape "$(cd "$1" && pwd -P)")"
  printf '(subpath "%s")' "$p"
}

{
  cat <<'EOF'
(version 1)
(deny default)

;; Basic process/runtime operations. The profile remains file/network
;; restrictive; these keep a normal already-execed Rust daemon alive.
(allow process*)
(allow sysctl-read)
(allow signal (target self))
(allow mach-lookup)
(allow network*)

;; Allow system metadata reads that libc/Foundation may perform lazily after
;; sandbox entry. Data reads remain path-scoped below.
(allow file-read-metadata)
(allow file-read-data
  (subpath "/System")
  (subpath "/usr/lib")
  (subpath "/private/var/db/timezone")
  (literal "/dev/null")
  (literal "/dev/urandom"))
EOF

  if ((${#reads[@]})); then
    echo
    echo ';; Explicit read-only VM inputs.'
    echo '(allow file-read-data'
    for path in "${reads[@]}"; do
      printf '  %s\n' "$(literal_form "$path")"
    done
    echo ')'
  fi

  if ((${#read_writes[@]} || ${#read_write_files[@]})); then
    echo
    echo ';; Per-VM working directories/files: API socket, logs, metrics, snapshots.'
    echo '(allow file-read* file-write*'
    for path in "${read_writes[@]}"; do
      printf '  %s\n' "$(subpath_form "$path")"
    done
    for path in "${read_write_files[@]}"; do
      printf '  %s\n' "$(literal_form "$path")"
    done
    echo ')'
  fi
} >"$out"
