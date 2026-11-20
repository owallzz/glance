# AGENTS.md

glance: peer-to-peer screen sharing rooms (multi-sharer, voice, chat, webcam, contacts) with a Discord-style UI. See `PLAN.md` for the roadmap and locked decisions, `README.md` for user-facing docs.

## Commands
| Task | Command |
|---|---|
| Host env (immutable Fedora only) | `source scripts/env.sh` |
| Build | `cargo build` |
| Run | `cargo run -p glance-app` (`-- --demo` for a populated fake room, `-- --spike N W H FPS` for the video spike) |
| Test peer | `cargo run -p glance-app -- bot <link> [name] [--stream WxH@FPS] [--tone] [--file PATH] [--ring-back] [--camera]` (headless: joins, greets, echoes chat, toggles mute, accepts contact requests and answers rings by joining; `--ring-back` instead rings a new contact back once they leave; `--file` also sends the file to anyone who becomes its contact; `--stream` shares a test pattern through the real encoder plus a 440 Hz tone as stream audio; `--tone` talks with a sine tone and prints who it hears speaking) |
| Capture probe | `cargo run --release -p glance-media --example capture_probe [secs] [max_height]` (captures, encodes, prints fps and encode cost) |
| Second identity | `GLANCE_DATA_DIR=/tmp/glance-b cargo run -p glance-app` |
| Auto-watch | `GLANCE_AUTOWATCH=1` watches every stream automatically (lets a second instance test viewing without clicks; a bot with it prints `hearing stream audio from ...`) |
| Encoder speed | `cargo run --release -p glance-media --example encode_bench -- 3840x2160 120 --pattern [--sw|--hw]` (encode ms/frame, decode+scale to a 1080p tile; `GLANCE_ENCODER=software|hardware` forces the encoder anywhere, `GLANCE_H264_THREADS=1` for single-threaded, `GLANCE_H264_LOG=1` for OpenH264's own log) |
| macOS app | `scripts/package-macos.sh <binary> <version> <out>` (macOS only) builds an ad-hoc signed glance.app (Info.plist in `packaging/macos/` has the mic/camera/screen usage strings and the glance:// scheme) and a .dmg. `glance --export-icons <dir>` writes the icon PNGs it uses. |
| Audio share probe | `cargo run -p glance-media --example stream_audio_probe -- [app-name]` lists shareable audio and captures it for 3 s. To test without sound playing aloud, route a tone into a temporary null sink: `pw-cli -m create-node adapter '{ factory.name=support.null-audio-sink node.name=glance-test-sink media.class=Audio/Sink object.linger=false }' &` then `pw-play --target glance-test-sink --properties '{ application.name="ToneApp" }' tone.wav &` |
| Skip portal | `GLANCE_PW_NODE=<id>` reads an existing PipeWire node instead of opening the share dialog (pair with a Mutter screencast for tests) |
| Local only | `GLANCE_RELAY=off` (no relays, direct addresses only); `GLANCE_RELAY=<url>` for a custom relay |
| Screenshot | `scripts/shot.sh out.png 2 --demo` (see `.claude/skills/run-glance`) |
| Test | `cargo test --workspace` |
| Lint | `cargo clippy --workspace --all-targets -- -D warnings` |
| Format | `cargo fmt --all` |

Run the tests and the linter before every commit; CI runs the same checks.

## Layout
- `crates/glance-app`: gpui-kit UI, binary `glance`.
  - `net.rs`: tokio runtime, identity key + profile.json in the data dir, helpers to run network futures off the UI thread. `session.rs`: start/join rooms and apply `RoomEvent`s to UI state. `bot.rs`: headless test peer.
  - `app.rs`: root view `GlanceApp` (all UI state, rail, user panel, key actions). Screens are `impl GlanceApp` blocks in `home.rs`, `room.rs`, and `overlays.rs` (Go Live, settings).
  - `voice.rs`: audio engine per room, mute/deafen, speaking rings, per-user volume, device and processing settings.
  - `streaming.rs`: Go Live (capture -> broadcast), own-stream preview, watching others (feed -> decoder -> tile).
  - `contacts.rs`: contact book (contacts.json), presence, calling/ringing contacts, incoming calls with ringtone and OS notification, missed calls, call history and the "call ended" add-contact prompt. Settings persist in settings.json (`app.rs`).
  - `tray.rs`: tray icon (ksni on Linux, tray-icon on macOS/Windows), close-to-tray (minimize while the tray is up), start on login via auto-launch (release builds only, never with `GLANCE_DATA_DIR`).
  - `files.rs`: send (attach/drag & drop), offers, downloads with progress into Downloads (`GLANCE_DOWNLOAD_DIR` overrides), inline image previews.
  - `popout.rs`: fullscreen (immersive) viewing and pop-out windows (a popped-out stream gets its own Watch/feed; closing the window brings it back).
  - `camera.rs`: webcam on/off (camera track broadcast at 640×360@24) and auto-watching everyone's camera into person tiles.
  - `ui.rs`: shared widgets (Avatar, icon_button, pill, live_badge).
  - `video.rs`: `VideoView` tile. `spike.rs`: the render benchmark.
  - `theme.rs` + `assets/themes/glance.json`: design tokens and the gpui-kit component theme (dark + light).
- `crates/glance-core`: view-model types the UI renders (`model.rs`), the persistent contact book with its state transitions (`contacts.rs`), formatting helpers, `demo.rs` fake data. Room/contacts state machines land here (no I/O, unit-tested).
- `crates/glance-net`: `Node` (iroh endpoint + router) and the room mesh actor (`room.rs`): membership, chat, state, voice as QUIC datagrams straight to an audio sink, chat/file history for late joiners, and video fan-out. `contacts.rs`: always-on contacts actor (ALPN `glance/contacts/1`): requests, presence, ring/answer, join requests; blocked peers refused, non-contacts can only send requests. `files.rs`: iroh-blobs store (on disk in the data dir), add/download/export, `unique_path` sanitizes peer-supplied names. (one QUIC uni stream per watcher, per-watcher queue that skips to keyframes when full). `tests/room_mesh.rs` runs in-process peers.
- `crates/glance-media`: voice (`audio/`: cpal devices via lock-free rings, speexdsp echo cancellation + RNNoise suppression, Opus with FEC/DTX, jitter buffer, mixer/engine thread), webcam capture via nokhwa (`camera.rs`), frames and scaling (`frame.rs`), H.264 (`codec.rs`: `VideoEncoder` picks a hardware encoder from `hw/` (NVENC via run-time loaded driver + vendored bindings in `hw/nvenc_sys.rs`) or falls back to OpenH264; decoding is always OpenH264), capture (`capture/`: Wayland = portal + PipeWire in `wayland.rs`; X11/macOS/Windows = zed-scap in `scap_backend.rs`), and `pipeline.rs` (broadcast: capture -> scale -> encode only while watched; decoder thread for viewers).
- `crates/glance-proto`: tickets (`glance://join/...`), room control messages and join proofs, length-prefixed postcard framing. Breaking wire changes get a new ALPN.
- `scripts/`: host build helpers and `shot.sh` (screenshots).
- `.claude/skills/run-glance`: how to launch, screenshot and measure the app.

## Conventions
- All UI goes through gpui-kit: `use gpui_kit::*;` for GPUI and `gpui_kit::component::*` for components. Never depend on `gpui`/`gpui-pre` crates directly.
- Colours come from `theme::Palette` or the gpui-kit theme, never inline hex in views. New colour needs → add a palette slot (dark and light).
- Design follows Discord's design system (layout, spacing, palette). Never use Discord's name, logo, sounds or the gg sans font. Use our own icon/logo and bundled OFL fonts.
- Icons: Lucide via `gpui_kit::assets::IconName`.
- Keep the UI thread free: capture, encode/decode, audio and networking run on their own threads or the tokio runtime, and talk to the UI over bounded channels.
- Lightweight is a feature: no work when nobody is watching, no background polling.
- Errors: `anyhow` in the app, `thiserror` in library crates.

## Workflow
- Commit in small steps, one idea per commit, imperative messages. Push after each commit and keep CI green.
- Tick off milestones in `PLAN.md` in the commit that finishes them.
- Git identity for this repo is set locally (`owallz`); don't change it.
- CI: Linux jobs run on the owallzz org's self-hosted runner (`runs-on: [self-hosted, Linux, X64]`) in a `fedora:44` container. macOS and Windows jobs run on Blacksmith (`blacksmith-6vcpu-macos-latest`, `blacksmith-4vcpu-windows-2025`), and only those platforms use Blacksmith. Never use GitHub-hosted runners. Blacksmith jobs don't use containers or the ownership-fix step; caching uses upstream `actions/cache` via `Swatinem/rust-cache` (Blacksmith backs it). Don't `sudo` in workflows. System packages go in the `dnf install` line. Keep the "Fix workspace ownership" step last in every job.
- Project skills live in `.claude/skills/`. Use them, and add one when a multi-step task keeps coming up.

## Releasing
1. Bump `version` in the root `Cargo.toml` (`[workspace.package]`), run `cargo build` to refresh `Cargo.lock`, commit, push, and wait for CI to pass on all three platforms.
2. `git tag -a vX.Y.Z -m "glance vX.Y.Z" && git push origin vX.Y.Z`. `release.yml` checks the tag against the version and attaches plain binaries (no archives) for Linux (self-hosted), macOS aarch64 and Windows x86_64 (Blacksmith).
3. Write release notes with `gh release edit vX.Y.Z --notes-file ...`: what works, what doesn't, and the unsigned-build workarounds.

## Privacy
This repo must not contain personal or identifying information. Never commit:
- real names, email addresses, phone numbers, or home addresses (also not in `authors` fields or code comments)
- absolute paths from a personal machine (e.g. `/home/<user>/...`); use relative paths or `$HOME`
- usernames, hostnames, IPs, or device names of personal machines
- API keys, tokens, passwords, or private keys (use `.env`, which is gitignored, plus a `.env.example` with placeholders)
- screenshots, logs, or test fixtures that show any of the above

Check the diff for these before every commit.

## Gotchas
- Don't open the real webcam in automated tests (it turns the user's camera light on); use `glance bot --camera` (synthetic camera) instead. Listing cameras is fine.
- Video is per track (`Track::Screen`, `Track::Camera`): watchers are keyed by (peer, track) and each uni stream starts with `[UNI_VIDEO, track]`.
- Room connections: only the smaller endpoint id dials (newcomers always dial the link's host); a duplicate is replaced gracefully (old connection's frames still accepted, closed after 5 s).
- When editing with scripted search/replace, assert that each pattern matched. `cargo fmt` reflows code, and a silent miss once hid a real bug (pop-out feeds never routed).
- Every request that makes a sharer send a keyframe must be rate-limited (≤1/s): keyframes are large, and a flood makes congestion worse.
- A gpui window can have helper X windows with the same title; when scripting, pick the one whose geometry matches the real window.
- Stock GNOME has no StatusNotifierItem host: the tray doesn't appear and closing the window quits (by design).
- Never `pkill -f` / `pgrep -f` with a pattern that appears in your own command (e.g. `glance bot`, a script name): it matches your shell and kills it. Use `pgrep -x glance` and pids you saved.
- xdotool `mousemove --window` misplaces the pointer for our client-decorated window; compute absolute coordinates from `getwindowgeometry --shell` (as `scripts/shot.sh` does).
- Linux capture needs PipeWire headers (`pipewire-devel`, or `brew install pipewire` on the immutable dev host) and clang for bindgen. zed-scap's own Wayland path is disabled: it needs libspa 0.8, which doesn't build against PipeWire 1.x.
- GNOME only sends screencast frames when the screen changes; the broadcaster re-encodes the last frame when a keyframe is requested.
- 4K60 end to end without the GUI: `glance bot new Host --stream 3840x2160@60` prints a link; `GLANCE_AUTOWATCH=decode glance bot <link> Viewer` decodes and prints fps.
- Hardware encoders must emit Constrained Baseline with SPS/PPS on every keyframe: viewers decode with OpenH264 and may join mid-stream. The hardware test in `codec.rs` skips itself without a GPU (CI).
- Measure video performance with release builds; glance crates run unoptimized in dev and the encoder pipeline is several times slower.
- Test multi-person flows with one GUI window plus `glance bot` peers. Two GUI windows overlap and GNOME ignores scripted window moves, so xdotool clicks hit the wrong one.
- The bot uses a fresh identity each run; the GUI uses the identity in its data dir. Two GUI instances need different `GLANCE_DATA_DIR`s: a second launch with the same data dir hands its arguments (a `glance://` link, or just "show") to the running one through `instance.json` and exits (`instance.rs`).
- `glance://` registration (`scheme.rs`: desktop entry on Linux, HKCU registry on Windows, Info.plist on macOS) and start-on-login only happen in release builds without `GLANCE_DATA_DIR`, so dev builds never take over the handler.
- Linux audio goes through cpal's PipeWire host (falls back to ALSA); settings only list driver-backed devices (`offered` in `audio/device.rs`). `cargo run -p glance-media --example audio_devices -- --raw` shows what the host reports.
- Key bindings: gpui-pre 0.3 has no `secondary-` modifier; use `SHORTCUTS` in `app.rs` (cmd on macOS, ctrl elsewhere). A test parses them, since a bad keystroke panics at startup.
- xdotool can't move keyboard focus on GNOME Wayland: `xdotool type` goes to whatever X11 window is active (maybe not glance). Only type after confirming `xdotool getactivewindow` is the glance window. Unfocused XWayland windows also stop repainting, so screenshots go stale.
- QUIC idle timeout is 10 s with 2 s keep-alives (`glance-net/src/lib.rs`): a crashed peer leaves rooms after ~10 s.
- The dev host is immutable Fedora without `-devel` packages: `source scripts/env.sh` first, or the fontconfig/xkbcommon build scripts fail.
- gpui-kit 0.10 pins exact `gpui-pre-*` versions. Upgrade gpui-kit as a whole, never individual gpui crates.
- Screenshots: use `scripts/shot.sh` (runs under XWayland). XWayland throttles background windows to ~1 fps, so measure video natively on Wayland.
- Video frames must be scaled to the tile size before upload (`TargetSize`/`FrameScaler`); uploading full 4K per frame costs most of the UI thread.
- In `impl GlanceApp` render helpers that are called inside `.map(...)` closures, return `AnyElement`, not `impl IntoElement`: edition-2024 return types capture `&mut self`/`cx` and the borrow checker rejects them.
- Layout containers that hold a `flex_1` child next to fixed-width panels need `.min_w_0()`, or their min-content width pushes panels off-screen.
- First build is slow (~2 min). Dependencies build at opt-level 2 in dev (see the workspace profile) so video paths stay usable in debug.
- Test modules in files that `use gpui_kit::*` must import items explicitly, not `use super::*`: the glob brings GPUI's `test` macro, which shadows `#[test]` ("recursion limit reached").
