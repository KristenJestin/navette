#!/usr/bin/env bash
# Installs the system libraries Navette links against, on an Ubuntu runner (CI and release).
# gliff's hypr-input links libxkbcommon; its Wayland crates are pure Rust and link nothing.
# Add a package here when a dependency starts linking one (libva comes with gliff-va).
set -euo pipefail
sudo apt-get update -q
sudo apt-get install -y -q --no-install-recommends libxkbcommon-dev
