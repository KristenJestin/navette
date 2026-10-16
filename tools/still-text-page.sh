#!/usr/bin/env bash
# Renders a still page of small text, invented, as a binary PPM: a bar, a terminal of coloured
# code and a page of prose, the way a work monitor looks. The page the sharpness measure encodes
# (`still_text_measure` in src/display/stream.rs). Needs pango-view and ImageMagick.
#
#   tools/still-text-page.sh <out.ppm> [width] [height]
set -euo pipefail

out=${1:?usage: tools/still-text-page.sh <out.ppm> [width] [height]}
width=${2:-2560}
height=${3:-1440}
half=$((width / 2))
# pango-view counts widths in points, at 96 dpi.
pt() { echo $(($1 * 72 / 96)); }
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

words=(lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor incididunt
  ut labore et dolore magna aliqua enim minim veniam quis nostrud exercitation ullamco laboris)
colours=('#e06c75' '#98c379' '#e5c07b' '#61afef' '#c678dd' '#56b6c2' '#abb2bf')
suffixes=('()' ';' ' =' '::' '_id' '[0]' ',' '')

# A fixed seed: the same page on every machine.
RANDOM=12
pick() { local -n list=$1; printf '%s' "${list[RANDOM % ${#list[@]}]}"; }

for line in $(seq 1 90); do
  printf '<span foreground="#5c6370">%4d</span>  ' "$line"
  for _ in $(seq 1 $((3 + RANDOM % 7))); do
    printf '<span foreground="%s">%s%s</span> ' "$(pick colours)" "$(pick words)" "$(pick suffixes)"
  done
  echo
done >"$work/code.txt"

for _ in $(seq 1 14); do
  for _ in $(seq 1 $((60 + RANDOM % 60))); do printf '%s ' "$(pick words)"; done
  printf '\n\n'
done >"$work/prose.txt"

echo '1 2 3 4 5     editor - notes.md     cpu 7%   mem 41%   wifi   Tue 09:41' >"$work/bar.txt"

pango-view -q --font='monospace 9' --background='#1a1b26' --foreground='#c0caf5' \
  --width="$(pt "$width")" --margin='4 12' -o "$work/bar.png" "$work/bar.txt"
pango-view -q --font='monospace 10' --markup --background='#282c34' --foreground='#abb2bf' \
  --width="$(pt $((half - 24)))" --margin=12 -o "$work/code.png" "$work/code.txt"
pango-view -q --font='sans 10' --background='#ffffff' --foreground='#202020' --wrap=word \
  --width="$(pt $((half - 48)))" --margin=24 -o "$work/prose.png" "$work/prose.txt"

magick -size "${width}x${height}" xc:'#1e1e2e' \
  "$work/bar.png" -geometry +0+0 -composite \
  "$work/code.png" -geometry +8+32 -composite \
  "$work/prose.png" -geometry +$((half + 8))+32 -composite \
  -crop "${width}x${height}+0+0" -depth 8 "ppm:$out"
