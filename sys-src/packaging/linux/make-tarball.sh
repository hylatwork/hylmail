#!/usr/bin/env bash
#
# Packs the two launchers into the relocatable tarball that the release page
# offers next to the `.deb` - useful when there is no root password, and the
# only Linux artifact that does not need `dpkg-deb` or `cargo-deb`.
#
# Usage:
#   packaging/linux/make-tarball.sh [VERSION] [BIN_DIR]
#
#   VERSION   defaults to the `[workspace.package]` version in Cargo.toml
#   BIN_DIR   defaults to `<repo>/target/release`
#
# The archive unpacks into a single `HylMail-<version>` directory whose `bin/`
# can be put on `$PATH`; `share/` carries the desktop entry and the icons for
# the sizes freedesktop.org knows about.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"

version="${1:-}"
if [ -z "$version" ]; then
    version="$(awk '/^\[workspace\.package\]/{inside=1} inside && /^version[[:space:]]*=/{gsub(/[",]/,"",$3); print $3; exit}' "$root/Cargo.toml")"
fi
if [ -z "$version" ]; then
    echo "make-tarball.sh: cannot determine the version" >&2
    exit 2
fi

bin_dir="${2:-$root/target/release}"
for binary in hylmail-gui hylmail-cli; do
    if [ ! -f "$bin_dir/$binary" ]; then
        echo "make-tarball.sh: $bin_dir/$binary is missing - build with 'cargo build --release -p hylmail-cli' first" >&2
        exit 2
    fi
done

arch="$(uname -m)"
case "$arch" in
    x86_64) triple="x86_64-unknown-linux-gnu" ;;
    aarch64 | arm64) triple="aarch64-unknown-linux-gnu" ;;
    *) triple="$arch-unknown-linux-gnu" ;;
esac

name="HylMail-$version"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
target="$stage/$name"

mkdir -p "$target/bin" "$target/share/applications" "$target/share/doc/hylmail"

install -m 0755 "$bin_dir/hylmail-gui" "$target/bin/hylmail-gui"
install -m 0755 "$bin_dir/hylmail-cli" "$target/bin/hylmail-cli"
install -m 0644 "$here/de.hylmail.HylMail.desktop" "$target/share/applications/de.hylmail.HylMail.desktop"
install -m 0644 "$root/README.md" "$target/share/doc/hylmail/README.md"
install -m 0644 "$root/LICENSE" "$target/share/doc/hylmail/LICENSE.txt"

for size in 16 24 32 48 64 128 256 512; do
    dir="$target/share/icons/hicolor/${size}x${size}/apps"
    mkdir -p "$dir"
    install -m 0644 "$here/../icons/hylmail-$size.png" "$dir/de.hylmail.HylMail.png"
done

mkdir -p "$root/dist"
archive="$root/dist/$name-$triple.tar.gz"
rm -f "$archive"
tar --owner=0 --group=0 --numeric-owner -czf "$archive" -C "$stage" "$name"

echo "make-tarball.sh: $archive"
