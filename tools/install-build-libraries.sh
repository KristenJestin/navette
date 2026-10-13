#!/bin/sh
# The system libraries Navette builds against, on an Ubuntu runner. The one list: the release and
# beta workflows call it, and `verify` in ci.yml, so that a new dependency (video,
# audio, input...) is added here once and not forgotten in one of the three builds.
#
#   tools/install-build-libraries.sh
set -eu

sudo apt-get update
sudo apt-get install -y --no-install-recommends \
  libclang-dev \
  libgbm-dev \
  libpipewire-0.3-dev \
  libva-dev \
  libwayland-dev \
  libxkbcommon-dev
