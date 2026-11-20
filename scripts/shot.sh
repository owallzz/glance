#!/usr/bin/env bash
# Screenshot the debug build of glance (Linux, X11 or XWayland).
#
#   scripts/shot.sh <out.png> <wait-secs> [glance args...]
#   CLICKS="x,y x,y" KEYS="Escape ctrl+shift+m" scripts/shot.sh out.png 2 --demo
#
# CLICKS are window-relative pixel coordinates, KEYS are xdotool key names;
# both run in order after the wait. Needs xdotool and ImageMagick `import`.
# On Wayland the app runs under XWayland so it can be captured; XWayland
# throttles unfocused windows, so fps shown in screenshots is not meaningful.
set -euo pipefail
out=$1 wait=$2
shift 2
cd "$(dirname "$0")/.."
WAYLAND_DISPLAY= ./target/debug/glance "$@" >/dev/null 2>&1 &
pid=$!
trap 'kill $pid 2>/dev/null || true' EXIT
id=""
for _ in $(seq 1 30); do
  id=$(xdotool search --pid "$pid" --name '^glance$' 2>/dev/null | head -1 || true)
  [ -n "$id" ] && break
  sleep 0.5
done
[ -n "$id" ] || { echo "glance window not found" >&2; exit 1; }
sleep "$wait"
# Window-relative coordinates -> absolute (`mousemove --window` misplaces
# the pointer for client-decorated windows under XWayland).
eval "$(xdotool getwindowgeometry --shell "$id")"
for c in ${CLICKS:-}; do
  xdotool mousemove "$((X + ${c%,*}))" "$((Y + ${c#*,}))" click 1
  sleep 0.7
done
for k in ${KEYS:-}; do
  xdotool key "$k"   # real input; `key --window` sends synthetic events gpui ignores
  sleep 0.5
done
sleep 1
import -window "$id" "$out"
echo "$out"
