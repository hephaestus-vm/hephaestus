#!/usr/bin/env bash
# Package a prebuilt Mach-O executable as a macOS app and sign it with the
# restricted com.apple.vm.networking entitlement. Restricted entitlements must
# be authorized by an embedded provisioning profile, so signing the standalone
# executable is not sufficient.

set -euo pipefail

usage() {
    cat >&2 <<'EOF'
usage: scripts/package-vmnet-app.sh <executable> <output.app>

Environment:
  HEPHAESTUS_PROVISIONING_PROFILE  Profile to embed. If unset, search Xcode's
                                   installed profiles for ca.nodegroup.hephaestus.
  HEPHAESTUS_SIGN_IDENTITY         Apple Development identity or SHA-1. If
                                   unset, use the first Apple Development identity.
  HEPHAESTUS_BUNDLE_ID             Bundle ID (default: ca.nodegroup.hephaestus).
EOF
    exit 2
}

[[ $# -eq 2 ]] || usage
executable="$1"
output_app="$2"
[[ -f "$executable" ]] || { echo "executable not found: $executable" >&2; exit 1; }

bundle_id="${HEPHAESTUS_BUNDLE_ID:-ca.nodegroup.hephaestus}"
identity="${HEPHAESTUS_SIGN_IDENTITY:-}"
if [[ -z "$identity" ]]; then
    identity="$(security find-identity -v -p codesigning | awk '/Apple Development/{print $2; exit}')"
fi
[[ -n "$identity" ]] || {
    echo "no Apple Development signing identity; set HEPHAESTUS_SIGN_IDENTITY" >&2
    exit 1
}

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
profile_tool="$script_dir/vmnet_profile.py"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

profile_matches() {
    local candidate="$1" decoded="$tmp/candidate.plist"
    security cms -D -i "$candidate" >"$decoded" 2>/dev/null || return 1
    python3 "$profile_tool" matches "$decoded" "$bundle_id"
}

profile="${HEPHAESTUS_PROVISIONING_PROFILE:-}"
if [[ -n "$profile" ]]; then
    [[ -f "$profile" ]] || { echo "provisioning profile not found: $profile" >&2; exit 1; }
    profile_matches "$profile" || {
        echo "profile does not authorize com.apple.vm.networking for $bundle_id: $profile" >&2
        exit 1
    }
else
    search_dirs=(
        "$HOME/Library/Developer/Xcode/UserData/Provisioning Profiles"
        "$HOME/Library/MobileDevice/Provisioning Profiles"
    )
    for dir in "${search_dirs[@]}"; do
        [[ -d "$dir" ]] || continue
        while IFS= read -r -d '' candidate; do
            if profile_matches "$candidate"; then
                profile="$candidate"
                break 2
            fi
        done < <(find "$dir" -maxdepth 1 -type f -print0)
    done
fi
[[ -n "$profile" ]] || {
    echo "no installed macOS profile authorizes com.apple.vm.networking for $bundle_id" >&2
    echo "set HEPHAESTUS_PROVISIONING_PROFILE to the downloaded profile" >&2
    exit 1
}

profile_plist="$tmp/profile.plist"
security cms -D -i "$profile" >"$profile_plist"

# Build only the entitlements this executable needs. The application and team
# identifiers must match the embedded profile; unrelated profile entitlements
# (for example keychain wildcard groups) should not be claimed by the binary.
python3 "$profile_tool" write-entitlements \
    "$profile_plist" "$tmp/entitlements.plist"

exec_name="$(basename "$executable")"
rm -rf "$output_app"
mkdir -p "$output_app/Contents/MacOS"
cp "$executable" "$output_app/Contents/MacOS/$exec_name"
chmod 755 "$output_app/Contents/MacOS/$exec_name"
cp "$profile" "$output_app/Contents/embedded.provisionprofile"

python3 "$profile_tool" write-info \
    "$output_app/Contents/Info.plist" "$bundle_id" "$exec_name"

codesign --force --sign "$identity" --timestamp=none \
    --entitlements "$tmp/entitlements.plist" "$output_app"
codesign --verify --strict "$output_app"

echo "packaged $output_app"
echo "  bundle:  $bundle_id"
echo "  profile: $(/usr/libexec/PlistBuddy -c 'Print :Name' "$profile_plist")"
echo "  identity: $identity"
