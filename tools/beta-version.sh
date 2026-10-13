#!/bin/sh
# The version of the beta of a workflow run: the patch after the last released version, as a
# pre-release numbered by the run (`0.2.4-beta.21`), so that it sorts after the release it was
# built on. Read from the `v*` tags of the repository it runs in; a pre-release never counts.
#
#   tools/beta-version.sh <run>
set -eu

case "${1:-}" in
  '' | *[!0-9]* | 0) echo "usage: tools/beta-version.sh <run>  (a positive integer)" >&2; exit 2 ;;
esac

last=$(git tag --list 'v*' | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | sort -V | tail -n 1 || true)
last=${last#v}
IFS=. read -r major minor patch <<EOT
${last:-0.0.0}
EOT
echo "$major.$minor.$((patch + 1))-beta.$1"
