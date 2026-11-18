#!/usr/bin/env bash
#
# Wraps `dist/HylMail.app` into `dist/HylMail-<version>.dmg`.
#
# Usage:
#   packaging/macos/make-dmg.sh [VERSION]
#
# Runs `make-app.sh` first unless HYLMAIL_SKIP_APP=1 is set, so the release job
# only has to call this script once.  `hdiutil` makes this macOS-only.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"

if ! command -v hdiutil >/dev/null 2>&1; then
    echo "make-dmg.sh: hdiutil is missing - a disk image can only be built on macOS" >&2
    exit 2
fi

version="${1:-}"
if [ -z "$version" ]; then
    version="$(awk '/^\[workspace\.package\]/{inside=1} inside && /^version[[:space:]]*=/{gsub(/[",]/,"",$3); print $3; exit}' "$root/Cargo.toml")"
fi
if [ -z "$version" ]; then
    echo "make-dmg.sh: cannot determine the version" >&2
    exit 2
fi

app="${HYLMAIL_APP:-$root/dist/HylMail.app}"
if [ "${HYLMAIL_SKIP_APP:-0}" != "1" ]; then
    "$here/make-app.sh" "$version"
fi
if [ ! -d "$app" ]; then
    echo "make-dmg.sh: $app is missing - run make-app.sh first" >&2
    exit 2
fi

mkdir -p "$root/dist"
dmg="$root/dist/HylMail-$version.dmg"
staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT

# A drag target next to the bundle, which is the layout users expect.  `/Applications`
# is a symlink so the volume only ever holds the bundle and that link.
cp -R "$app" "$staging/HylMail.app"
ln -s /Applications "$staging/Applications"

rm -f "$dmg"
hdiutil create \
    -volname "HylMail $version" \
    -srcfolder "$staging" \
    -ov -format UDZO \
    "$dmg"

echo "make-dmg.sh: $dmg"
