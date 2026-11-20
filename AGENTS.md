# AGENTS.md

glance: peer-to-peer screen sharing rooms (multi-sharer, voice, chat, webcam, contacts) with a Discord-style UI. See `PLAN.md` for the roadmap and locked decisions, `README.md` for user-facing docs.

## Commands
| Task | Command |
|---|---|
| Host env (immutable Fedora only) | `source scripts/env.sh` |
| Build | `cargo build` |
| Run | `cargo run -p glance-app` (`-- --demo` for a populated fake room, `-- --spike N W H FPS` for the video spike) |
| Screenshot | `scripts/shot.sh out.png 2 --demo` (see `.claude/skills/run-glance`) |
| Test | `cargo test --workspace` |
| Lint | `cargo clippy --workspace --all-targets -- -D warnings` |
| Format | `cargo fmt --all` |

Run the tests and the linter before every commit; CI runs the same checks.

## Layout
- `crates/glance-app`: gpui-kit UI, binary `glance`.
  - `app.rs`: root view `GlanceApp` (all UI state, rail, user panel, key actions). Screens are `impl GlanceApp` blocks in `home.rs`, `room.rs`, and `overlays.rs` (Go Live, settings).
  - `ui.rs`: shared widgets (Avatar, icon_button, pill, live_badge).
  - `video.rs`: `VideoView` tile. `spike.rs`: the render benchmark.
  - `theme.rs` + `assets/themes/glance.json`: design tokens and the gpui-kit component theme (dark + light).
- `crates/glance-core`: view-model types the UI renders (`model.rs`), formatting helpers, `demo.rs` fake data. Room/contacts state machines land here (no I/O, unit-tested).
- `crates/glance-net`: iroh endpoint, room mesh, media streams, contacts protocol.
- `crates/glance-media`: `VideoFrame`, `TargetSize` and `FrameScaler` (scale frames to tile size), the test `pattern`. Capture, codecs and audio land here behind per-OS traits.
- `crates/glance-proto`: wire messages, framing, tickets.
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
- CI runs on the owallzz org's self-hosted runner (`runs-on: [self-hosted, Linux, X64]`) in a `fedora:44` container. Never use GitHub-hosted runners. Don't `sudo` in workflows. System packages go in the `dnf install` line. Keep the "Fix workspace ownership" step last in every job.
- Project skills live in `.claude/skills/`. Use them, and add one when a multi-step task keeps coming up.

## Privacy
This repo must not contain personal or identifying information. Never commit:
- real names, email addresses, phone numbers, or home addresses (also not in `authors` fields or code comments)
- absolute paths from a personal machine (e.g. `/home/<user>/...`); use relative paths or `$HOME`
- usernames, hostnames, IPs, or device names of personal machines
- API keys, tokens, passwords, or private keys (use `.env`, which is gitignored, plus a `.env.example` with placeholders)
- screenshots, logs, or test fixtures that show any of the above

Check the diff for these before every commit.

## Gotchas
- The dev host is immutable Fedora without `-devel` packages: `source scripts/env.sh` first, or the fontconfig/xkbcommon build scripts fail.
- gpui-kit 0.10 pins exact `gpui-pre-*` versions. Upgrade gpui-kit as a whole, never individual gpui crates.
- Screenshots: use `scripts/shot.sh` (runs under XWayland). XWayland throttles background windows to ~1 fps, so measure video natively on Wayland.
- Video frames must be scaled to the tile size before upload (`TargetSize`/`FrameScaler`); uploading full 4K per frame costs most of the UI thread.
- In `impl GlanceApp` render helpers that are called inside `.map(...)` closures, return `AnyElement`, not `impl IntoElement`: edition-2024 return types capture `&mut self`/`cx` and the borrow checker rejects them.
- Layout containers that hold a `flex_1` child next to fixed-width panels need `.min_w_0()`, or their min-content width pushes panels off-screen.
- First build is slow (~2 min). Dependencies build at opt-level 2 in dev (see the workspace profile) so video paths stay usable in debug.
- Test modules in files that `use gpui_kit::*` must import items explicitly, not `use super::*`: the glob brings GPUI's `test` macro, which shadows `#[test]` ("recursion limit reached").
