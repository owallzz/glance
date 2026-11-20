# AGENTS.md

glance: peer-to-peer screen sharing rooms (multi-sharer, voice, chat, webcam, contacts) with a Discord-style UI. See `PLAN.md` for the roadmap and locked decisions, `README.md` for user-facing docs.

## Commands
| Task | Command |
|---|---|
| Host env (immutable Fedora only) | `source scripts/env.sh` |
| Build | `cargo build` |
| Run | `cargo run -p glance-app` |
| Test | `cargo test --workspace` |
| Lint | `cargo clippy --workspace --all-targets -- -D warnings` |
| Format | `cargo fmt --all` |

Run the tests and the linter before every commit; CI runs the same checks.

## Layout
- `crates/glance-app`: gpui-kit UI, binary `glance`. `src/theme.rs` holds the design tokens; `assets/themes/glance.json` is the gpui-kit component theme (dark + light).
- `crates/glance-core`: pure room/contacts state machines (no I/O, unit-tested).
- `crates/glance-net`: iroh endpoint, room mesh, media streams, contacts protocol.
- `crates/glance-media`: capture, video codecs, audio. Per-OS backends behind traits.
- `crates/glance-proto`: wire messages, framing, tickets.
- `scripts/`: host build helpers.

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
- Screenshots on GNOME Wayland: run with `WAYLAND_DISPLAY=` (XWayland) and use `xdotool search --name '^glance$'` + `import -window <id> out.png`.
- First build is slow (~2 min). Dependencies build at opt-level 2 in dev (see the workspace profile) so video paths stay usable in debug.
- Test modules in files that `use gpui_kit::*` must import items explicitly, not `use super::*`: the glob brings GPUI's `test` macro, which shadows `#[test]` ("recursion limit reached").
