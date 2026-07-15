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

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

profile_matches() {
    local candidate="$1" decoded="$tmp/candidate.plist"
    security cms -D -i "$candidate" >"$decoded" 2>/dev/null || return 1
    python3 - "$decoded" "$bundle_id" <<'PY'
import datetime
import plistlib
import sys

with open(sys.argv[1], "rb") as f:
    profile = plistlib.load(f)
entitlements = profile.get("Entitlements", {})
app_id = entitlements.get("com.apple.application-identifier") or entitlements.get("application-identifier", "")
platforms = profile.get("Platform", [])
expires = profile.get("ExpirationDate")
now = datetime.datetime.now(datetime.timezone.utc)
if expires is not None and expires.tzinfo is None:
    expires = expires.replace(tzinfo=datetime.timezone.utc)
ok = (
    app_id.endswith("." + sys.argv[2])
    and entitlements.get("com.apple.vm.networking") is True
    and "OSX" in platforms
    and (expires is None or expires > now)
)
raise SystemExit(0 if ok else 1)
PY
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
python3 - "$profile_plist" "$tmp/entitlements.plist" <<'PY'
import plistlib
import sys

with open(sys.argv[1], "rb") as f:
    profile = plistlib.load(f)
p = profile["Entitlements"]
app_id = p.get("com.apple.application-identifier") or p.get("application-identifier")
team_id = p["com.apple.developer.team-identifier"]
entitlements = {
    "com.apple.application-identifier": app_id,
    "com.apple.developer.team-identifier": team_id,
    "com.apple.security.virtualization": True,
    "com.apple.vm.networking": True,
}
with open(sys.argv[2], "wb") as f:
    plistlib.dump(entitlements, f, sort_keys=True)
PY

exec_name="$(basename "$executable")"
rm -rf "$output_app"
mkdir -p "$output_app/Contents/MacOS"
cp "$executable" "$output_app/Contents/MacOS/$exec_name"
chmod 755 "$output_app/Contents/MacOS/$exec_name"
cp "$profile" "$output_app/Contents/embedded.provisionprofile"

python3 - "$output_app/Contents/Info.plist" "$bundle_id" "$exec_name" <<'PY'
import plistlib
import sys

info = {
    "CFBundleExecutable": sys.argv[3],
    "CFBundleIdentifier": sys.argv[2],
    "CFBundleName": "Hephaestus",
    "CFBundlePackageType": "APPL",
    "CFBundleShortVersionString": "1.0",
    "CFBundleVersion": "1",
    "LSBackgroundOnly": True,
}
with open(sys.argv[1], "wb") as f:
    plistlib.dump(info, f, sort_keys=True)
PY

codesign --force --sign "$identity" --timestamp=none \
    --entitlements "$tmp/entitlements.plist" "$output_app"
codesign --verify --strict "$output_app"

echo "packaged $output_app"
echo "  bundle:  $bundle_id"
echo "  profile: $(/usr/libexec/PlistBuddy -c 'Print :Name' "$profile_plist")"
echo "  identity: $identity"
