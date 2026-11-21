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
# The generator at a fixed commit, checked against its hash: its output
# decides which crate sources go into the Flatpak. To upgrade, update both.
gen_rev=41c20aa10819cdb2a4f3ca171758a96d1955c018
gen_sha256=0a2db6be87d75910facef28ab46d4d6460802e8419ab850d0caa6a364d26b380
curl -fsSLo "$work/flatpak-cargo-generator.py" \
  "https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/$gen_rev/cargo/flatpak-cargo-generator.py"
echo "$gen_sha256  $work/flatpak-cargo-generator.py" | sha256sum -c --quiet -
"$work/venv/bin/python" -I "$work/flatpak-cargo-generator.py" Cargo.lock -o "$out"
echo "$out"
