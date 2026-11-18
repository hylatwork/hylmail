#!/usr/bin/env bash
#
# Assembles `dist/HylMail.app` from an already built pair of launchers.
#
# Usage:
#   packaging/macos/make-app.sh [VERSION] [BIN_DIR]
#
#   VERSION   defaults to the `[workspace.package]` version in Cargo.toml
#   BIN_DIR   defaults to `<repo>/target/release`
#
# Environment:
#   HYLMAIL_SIGN_IDENTITY   when set (and `codesign` exists) the bundle is
#                          signed with `codesign --force --deep --sign IDENTITY`
#   HYLMAIL_APP             output path, defaults to `<repo>/dist/HylMail.app`
#
# Run it on macOS after `cargo build --release -p hylmail-cli`; it is plain
# POSIX shell plus `install`/`cp`, so it also runs elsewhere (the CI job and the
# packaging test use that to check the bundle layout).
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"

version="${1:-}"
if [ -z "$version" ]; then
    version="$(awk '/^\[workspace\.package\]/{inside=1} inside && /^version[[:space:]]*=/{gsub(/[",]/,"",$3); print $3; exit}' "$root/Cargo.toml")"
fi
if [ -z "$version" ]; then
    echo "make-app.sh: cannot determine the version" >&2
    exit 2
fi

bin_dir="${2:-$root/target/release}"
app="${HYLMAIL_APP:-$root/dist/HylMail.app}"
contents="$app/Contents"

for binary in hylmail-gui hylmail-cli; do
    if [ ! -f "$bin_dir/$binary" ]; then
        echo "make-app.sh: $bin_dir/$binary is missing - build with 'cargo build --release -p hylmail-cli' first" >&2
        exit 2
    fi
done

rm -rf "$app"
mkdir -p "$contents/MacOS" "$contents/Resources"

install -m 0755 "$bin_dir/hylmail-gui" "$contents/MacOS/hylmail-gui"
install -m 0755 "$bin_dir/hylmail-cli" "$contents/MacOS/hylmail-cli"

# The Info.plist template carries {{VERSION}} twice; sed keeps this script free
# of a Python or plist dependency.
sed "s/{{VERSION}}/$version/g" "$here/Info.plist" >"$contents/Info.plist"
printf 'APPL????' >"$contents/PkgInfo"

install -m 0644 "$here/../icons/HylMail.icns" "$contents/Resources/HylMail.icns"
install -m 0644 "$root/LICENSE" "$contents/Resources/LICENSE.txt"

# The bundle identifier in the plist has to match the one the window reports;
# a mismatch makes the dock icon and the "Quit" menu entry behave oddly.
plist_id="$(sed -n '/CFBundleIdentifier/{n;s/.*<string>\(.*\)<\/string>.*/\1/p;}' "$contents/Info.plist")"
if [ "$plist_id" != "de.hylmail.HylMail" ]; then
    echo "make-app.sh: unexpected CFBundleIdentifier '$plist_id'" >&2
    exit 2
fi

if [ -n "${HYLMAIL_SIGN_IDENTITY:-}" ]; then
    if command -v codesign >/dev/null 2>&1; then
        codesign --force --deep --sign "$HYLMAIL_SIGN_IDENTITY" "$app"
    else
        echo "make-app.sh: HYLMAIL_SIGN_IDENTITY is set but codesign is unavailable" >&2
        exit 2
    fi
fi

if command -v plutil >/dev/null 2>&1; then
    plutil -lint "$contents/Info.plist" >/dev/null
fi

echo "make-app.sh: $app ($version)"
