#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
flatpak-builder \
  --force-clean \
  --user \
  --install \
  build-dir \
  flatpak/io.github.mars7x.hardware-info-gtk4.yml
