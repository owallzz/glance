#!/usr/bin/env bash
# Generate packaging/flatpak/cargo-sources.json (the offline crate sources
# flatpak-builder needs) from Cargo.lock. Rerun whenever Cargo.lock changes.
#   scripts/flatpak-cargo-sources.sh [out.json]
# Needs python3 and network access; installs the generator's Python deps
# into a throwaway venv.
set -euo pipefail
out=${1:-packaging/flatpak/cargo-sources.json}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
python3 -m venv "$work/venv"
"$work/venv/bin/pip" install --quiet 'aiohttp>=3.9.5,<4' 'tomlkit>=0.13.3,<1' 'PyYAML>=6,<7'
curl -fsSLo "$work/flatpak-cargo-generator.py" \
  https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/master/cargo/flatpak-cargo-generator.py
"$work/venv/bin/python" -I "$work/flatpak-cargo-generator.py" Cargo.lock -o "$out"
echo "$out"
