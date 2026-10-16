#!/usr/bin/env bash
# Installs Navette for the current user, from the folder of the release archive this script is in:
# the binary, the icon and the systemd user units. No root. It enables nothing and starts nothing;
# run it again to update (the same result).
set -euo pipefail

if [ "$(id -u)" -eq 0 ]; then
    echo "install.sh: do not run it as root: Navette installs for your own user." >&2
    exit 1
fi

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
bin="$HOME/.local/bin"
icons="$HOME/.local/share/icons/hicolor/scalable/apps"
units="$HOME/.config/systemd/user"

# (source file, destination folder, mode) for each thing installed.
files=(
    "navette|$bin|755"
    "navette.svg|$icons|644"
    "navette.service|$units|644"
    "navette-agent.service|$units|644"
)

# All of them there before the first is copied, so that a half-unpacked folder installs nothing.
for entry in "${files[@]}"; do
    name=${entry%%|*}
    [ -f "$here/$name" ] || { echo "install.sh: $name is not next to this script ($here)." >&2; exit 1; }
done

for entry in "${files[@]}"; do
    IFS='|' read -r name dir mode <<<"$entry"
    install -Dm"$mode" "$here/$name" "$dir/$name"
    echo "installed $dir/$name"
done

systemctl --user daemon-reload || {
    echo "install.sh: the files are installed, but 'systemctl --user daemon-reload' failed:" >&2
    echo "run it from your desktop session." >&2
    exit 1
}

cat <<'NEXT'

Next, on each machine:
  1. navette pair, once: `navette pair --listen <LAN-address:port>` at home, which shows a PIN,
     then `navette pair <home-address:port>` on the remote machine, which asks for it.
  2. systemctl --user enable --now navette

navette-agent.service is kept for one minor version; navette.service replaces it.
NEXT
