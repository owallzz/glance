# glance

Peer-to-peer screen sharing rooms with a familiar, Discord-style interface. Start a room, send the link, and everyone in it can go live at the same time. There are no accounts and no media server: peers connect directly over [iroh](https://iroh.computer) (QUIC, end-to-end encrypted, relay fallback).

> Early development. See [PLAN.md](PLAN.md) for the roadmap.

## Build and run

Requirements: a recent stable Rust toolchain, plus the gpui system libraries on Linux (`fontconfig`, `freetype`, `libxkbcommon`, `libxcb`, `wayland` development packages).

```sh
cargo run -p glance-app          # launches the `glance` binary
```

On an immutable Fedora host (Silverblue, Bazzite, ...) without `-devel` packages, source the helper first. It needs Homebrew's `fontconfig` for the pkg-config files:

```sh
source scripts/env.sh
cargo run -p glance-app
```

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
