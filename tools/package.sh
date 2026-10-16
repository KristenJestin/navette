#!/bin/sh
# Packs a built `navette` with its systemd user units, its icon, `install.sh` and a short install note,
# and writes the SHA-256 checksum beside the archive. Prints the archive's file name.
#
#   tools/package.sh <version> <binary> <out-dir>
set -eu

[ "$#" -eq 3 ] || { echo "usage: tools/package.sh <version> <binary> <out-dir>" >&2; exit 2; }
version=$1 binary=$2 out=$3
root=$(cd "$(dirname "$0")/.." && pwd)

name="navette-$version-linux-x86_64"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

mkdir "$stage/$name"
install -m 755 "$binary" "$stage/$name/navette"
install -m 644 "$root/dist/navette.service" "$stage/$name/navette.service"
install -m 644 "$root/dist/navette-agent.service" "$stage/$name/navette-agent.service"
install -m 755 "$root/dist/install.sh" "$stage/$name/install.sh"
install -m 644 "$root/assets/brand/icon-pixel.svg" "$stage/$name/navette.svg"
install -m 644 "$root/dist/INSTALL.md" "$stage/$name/INSTALL.md"

mkdir -p "$out"
tar --owner=0 --group=0 -C "$stage" -czf "$out/$name.tar.gz" "$name"
# Relative name, so that `sha256sum --check` works from the folder the two files are in.
(cd "$out" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
echo "$name.tar.gz"
