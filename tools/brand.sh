#!/usr/bin/env bash
# Renders the PNGs of assets/brand from its SVGs, with the fonts of assets/brand/fonts only: a
# fontconfig made for the run, so no font of the machine changes the drawing. Needs rsvg-convert
# (librsvg). The PNGs are committed; run this again after changing an SVG.
set -euo pipefail

brand="$(cd "$(dirname "$0")/../assets/brand" && pwd)"
run="$(mktemp -d)"
trap 'rm -rf "$run"' EXIT

cat >"$run/fonts.conf" <<EOF
<?xml version="1.0"?>
<!DOCTYPE fontconfig SYSTEM "fonts.dtd">
<fontconfig><dir>$brand/fonts</dir><cachedir>$run/cache</cachedir></fontconfig>
EOF
export FONTCONFIG_FILE="$run/fonts.conf"

for size in 128 256 512 1024; do
  rsvg-convert -w "$size" -h "$size" "$brand/icon.svg" -o "$brand/png/icon-$size.png"
done
rsvg-convert -w 512 -h 512 "$brand/icon-full.svg" -o "$brand/png/icon-full-512.png"
for size in 16 32; do
  rsvg-convert -w "$size" -h "$size" "$brand/favicon.svg" -o "$brand/png/favicon-$size.png"
done
# The pixel icon is 38 cells wide: rendered at whole multiples, so every cell has the same size.
for size in 266 532; do
  rsvg-convert -w "$size" -h "$size" "$brand/icon-pixel.svg" -o "$brand/png/icon-pixel-$size.png"
done
rsvg-convert "$brand/social-preview.svg" -o "$brand/png/social-preview.png"
