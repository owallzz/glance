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

## Testing a live room (GUI + bots)

1. Pre-create a profile so the first-run name card doesn't block: `mkdir -p /tmp/x/a && echo '{"name":"Ana","color":1752220}' > /tmp/x/a/profile.json`.
2. Launch: `GLANCE_DATA_DIR=/tmp/x/a GLANCE_RELAY=off RUST_LOG=glance=info WAYLAND_DISPLAY= ./target/debug/glance > /tmp/x/a.log 2>&1 &`.
3. Find the window (`xdotool search --name '^glance$'`), wait ~4 s for first paint, click "Start a room" at `423,241`. The link is logged as `room link: glance://join/...`.
4. Add peers: `GLANCE_RELAY=off ./target/debug/glance bot "$link" Robo &` (repeat with other names). Bots greet, echo chat, and toggle mute every 5 s.
5. Type into chat: click `1040,790`, then `xdotool type "..."` and `xdotool key Return`. Screenshot with `import -window <id>`.
6. Drop `GLANCE_RELAY=off` to test the real n0 relay path.

## Measuring video (spike)

`cargo build --release -p glance-app`, then run natively on Wayland (not XWayland, which throttles to ~1 fps):
`./target/release/glance --spike <tiles> <width> <height> <fps>`. It prints painted fps and frame size per tile to stderr every 2 s. Measure CPU via `/proc/<pid>/task/*/stat`.

## Gotchas
- `xdotool windowactivate --sync` / `windowsize --sync` hang on GNOME Wayland; don't use them.
- `xdotool key --window` sends synthetic events that gpui ignores; use plain `xdotool key` after clicking the window.
