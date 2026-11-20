---
name: run-glance
description: Build, launch and screenshot the glance app to check a UI change in the real app (home, demo room, Go Live, settings), or measure video rendering with the spike. Use when asked to run/screenshot glance or verify a UI change visually.
---

# Run and screenshot glance

1. Build: `source scripts/env.sh && cargo build -p glance-app` (the env script is only needed on immutable Fedora hosts; harmless elsewhere).
2. Screenshot with `scripts/shot.sh <out.png> <wait-secs> [args]`. Save outputs outside the repo (a scratch dir), never commit screenshots.
   - Home: `scripts/shot.sh /tmp/x/home.png 2`
   - Populated room (fake participants, streams, chat): `scripts/shot.sh /tmp/x/room.png 2 --demo`
   - Click/press after launch: `CLICKS="529,782" KEYS="Escape" scripts/shot.sh ... --demo`. Coordinates are window pixels at the default 1200×760 size; find them from a previous screenshot.
   - Useful points in `--demo` (default size): Go Live button `529,782`, settings gear `313,795`, first "Watch Stream" `427,334`.
3. Look at the PNG with the Read tool and compare against the Discord-style layout.
4. Kill leftovers with `pgrep -x glance | xargs -r kill`. Never `pkill -f target/debug/glance`: it matches your own shell command and kills it.

## Measuring video (spike)

`cargo build --release -p glance-app`, then run natively on Wayland (not XWayland, which throttles to ~1 fps):
`./target/release/glance --spike <tiles> <width> <height> <fps>`. It prints painted fps and frame size per tile to stderr every 2 s. Measure CPU via `/proc/<pid>/task/*/stat`.

## Gotchas
- `xdotool windowactivate --sync` / `windowsize --sync` hang on GNOME Wayland; don't use them.
- `xdotool key --window` sends synthetic events that gpui ignores; use plain `xdotool key` after clicking the window.
