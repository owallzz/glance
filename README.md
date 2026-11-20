# glance

Peer-to-peer screen sharing rooms with a familiar, Discord-style interface. Start a room, send the link, and everyone in it can go live at the same time. There are no accounts and no media server: peers connect directly over [iroh](https://iroh.computer) (QUIC, end-to-end encrypted, relay fallback).

## What it does

- **Rooms:** start one and the invite link is on your clipboard. Clicking a `glance://` link opens glance (or hands the link to the copy already running) and joins.
- **Screen sharing:** anyone can go live at once, at up to 1440p60 in software (4K at about 40 fps), with an optional app or system audio track. Viewers watch in a grid, focus, fullscreen or a pop-out window, with per-stream volume and stats.
- **Voice and webcams:** echo cancellation, noise suppression, per-person volume, speaking rings, mute/deafen with sounds.
- **Chat and files:** room chat; drop, attach or paste files and screenshots. Files go peer to peer and anyone who downloaded one helps serve it.
- **Contacts:** add people you've called, see when they're online or in a call, ring them, send them files outside calls. glance waits in the tray and starts on login.
- **Settings:** devices, light/dark theme, a custom relay server, start on login.

See [PLAN.md](PLAN.md) for the roadmap. Not done yet: hardware encoding on AMD/Intel GPUs under Windows (NVIDIA uses NVENC, Linux AMD/Intel use VA-API, Macs VideoToolbox; others use software) and signed installers.

## Install

Download the file for your system from the [latest release](../../releases/latest):

- **Linux:** the `linux-x86_64` binary. `chmod +x` it and run it. The first launch of a release build registers `glance://` links and a desktop entry with an icon.
- **macOS (Apple Silicon):** `glance-…-macos-aarch64.dmg`, drag glance to Applications. It isn't notarized yet, so right-click > Open the first time.
- **Windows:** the `.exe`. SmartScreen may warn because it isn't signed yet.

## Build and run

Requirements: a recent stable Rust toolchain, plus the gpui system libraries on Linux (`fontconfig`, `freetype`, `libxkbcommon`, `libxcb`, `wayland` development packages).

```sh
cargo run -p glance-app               # launches the `glance` binary
cargo run -p glance-app -- --demo     # start in a fake room (UI preview)
cargo run -p glance-app -- join <link> # open and join a room
cargo run -p glance-app -- bot <link>  # headless test peer
```

On an immutable Fedora host (Silverblue, Bazzite, ...) without `-devel` packages, source the helper first. It needs Homebrew's `fontconfig` for the pkg-config files:

```sh
source scripts/env.sh
cargo run -p glance-app
```

Start a room and the invite link is copied to your clipboard. Send it to someone; they paste it into the Join box (or run `glance join <link>`).

To try it alone, run a second identity with `GLANCE_DATA_DIR=/tmp/glance-b cargo run -p glance-app`, or add test peers with `glance bot <link>`.

## Test

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

## Layout

| Crate | What it holds |
|---|---|
| `glance-app` | gpui-kit UI and the `glance` binary |
| `glance-core` | room and contacts state machines |
| `glance-net` | iroh networking |
| `glance-media` | capture, codecs and audio |
| `glance-proto` | wire types shared by peers |
