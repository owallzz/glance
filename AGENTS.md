# AGENTS.md

glance: peer-to-peer screen sharing rooms (multi-sharer, voice, chat, webcam, contacts) with a Discord-style UI. See `PLAN.md` for the roadmap and locked decisions, `README.md` for user-facing docs.

## Commands
| Task | Command |
|---|---|
| Host env (immutable Fedora only) | `source scripts/env.sh` |
| Build | `cargo build` |
| Run | `cargo run -p glance-app` (`-- --demo` for a populated fake room, `-- --spike N W H FPS` for the video spike) |
| Test peer | `cargo run -p glance-app -- bot <link> [name] [--stream WxH@FPS] [--switch WxH@FPS [--switch-after SECS]] [--tone] [--file PATH] [--ring-back] [--camera] [--mute NAME]` (headless: joins, greets, echoes chat, echoes each DM from a contact once (`echo: <text>`), toggles mute, accepts contact requests and answers rings by joining; `--ring-back` instead rings a new contact back once they leave; `--file` also sends the file to anyone who becomes its contact; `--stream` shares a test pattern through the real encoder plus a 440 Hz tone as stream audio; `--switch` changes the live stream later (a smaller size of the same shape changes quality on the same capture, any other size swaps in a new source; prints `switched stream to ... (same source|new source)`, and a decoding viewer prints `stream is WxH`); `--tone` talks with a sine tone and prints `speaking:` (who it hears) and `audible:` (who reaches its speakers after local mute and deafen); `--mute NAME` mutes that person for the bot only) |
| Version | `glance --version` (or `-V`) |
| Updater | `glance update [--apply] [--restart]` checks and downloads from a terminal; `--apply` installs the staged update. Test against a fake API with `GLANCE_UPDATE_URL=<base> GLANCE_UPDATE_CURRENT_VERSION=0.0.1 GLANCE_UPDATE_PUBKEY="$(cat test.pub)"` (keys and signatures from `cargo run -p glance-app --example sign_update -- <dir> <files>`). `GLANCE_UPDATE=off` disables updates. |
| Sounds | `cargo run --release -p glance-media --example render_sounds -- <dir>` (WAVs plus peak, loudness, spectrum and loop-seam analysis; `--play <device>` plays the call tones through the real path, only onto an existing device such as the null sink below) |
| Capture probe | `cargo run --release -p glance-media --example capture_probe [secs] [max_height]` (captures, encodes, prints fps and encode cost) |
| Second identity | `GLANCE_DATA_DIR=/tmp/glance-b cargo run -p glance-app` |
| Auto-watch | `GLANCE_AUTOWATCH=1` watches every stream automatically (lets a second instance test viewing without clicks; a bot with it prints `hearing stream audio from ...`) |
| Encoder speed | `cargo run --release -p glance-media --example encode_bench -- 3840x2160 120 --pattern [--sw|--hw]` (encode ms/frame, decode+scale to a 1080p tile; `GLANCE_ENCODER=software|hardware|nvenc|vaapi|videotoolbox|mediafoundation` forces the encoder anywhere (tests too), `GLANCE_DECODER=software|hardware` the viewer's decoder, `GLANCE_H264_THREADS=1` for single-threaded, `GLANCE_H264_LOG=1` for OpenH264's own log) |
| macOS app | `scripts/package-macos.sh <binary> <version> <out>` (macOS only) builds an ad-hoc signed glance.app (Info.plist in `packaging/macos/` has the mic/camera/screen usage strings and the glance:// scheme) and a .dmg. `glance --export-icons <dir>` writes the icon PNGs it uses. |
| AppImage | `scripts/package-appimage.sh <binary> <out.AppImage>` (set `APPIMAGE_EXTRACT_AND_RUN=1` without FUSE). Pins linuxdeploy and the type2 runtime by tag and sha256. |
| Flatpak | `scripts/flatpak-cargo-sources.sh` (rerun when `Cargo.lock` changes; pins the generator by commit and sha256), then `flatpak-builder --user --install-deps-from=flathub --force-clean --install build-dir packaging/flatpak/io.github.owallzz.glance.yml` |
| Windows installer | `iscc /DAppVersion=X.Y.Z /DSourceExe=path\to\glance.exe packaging\windows\glance.iss` (Inno Setup) |
| Audio share probe | `cargo run -p glance-media --example stream_audio_probe -- [app-name]` lists shareable audio and captures it for 3 s. To test without sound playing aloud, route a tone into a temporary null sink: `pw-cli -m create-node adapter '{ factory.name=support.null-audio-sink node.name=glance-test-sink media.class=Audio/Sink object.linger=false }' &` then `pw-play --target glance-test-sink --properties '{ application.name="ToneApp" }' tone.wav &`. `--self-tone` plays from the probe itself, which "Entire system" must leave out (`pw-link -l` shows no link from it to `glance-stream-audio`). Don't `pkill -f glance-test-sink`: it matches the shell running it. |
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
  - `voice.rs`: audio engine per room, mute/deafen, speaking rings, per-user volume and local mute (persisted in settings.json as `peer_audio`, keyed by peer id hex, via `glance_core::voice::PeerAudio`; all pushed into the engine when voice starts), device and processing settings.
  - `streaming.rs`: Go Live (capture -> broadcast), own-stream preview, watching others (feed -> decoder -> tile).
  - `contacts.rs`: contact book (contacts.json), presence, calling/ringing contacts, outgoing ringback while a ring is unanswered (`AudioEngine::set_ringback`; leaving or timing out sends `CancelRing`), incoming calls with ringtone and OS notification, missed calls, call history and the "call ended" add-contact prompt. Settings persist in settings.json (`app.rs`).
  - `tray.rs`: tray icon (ksni on Linux, tray-icon on macOS/Windows), close-to-tray (minimize while the tray is up), start on login via auto-launch (release builds only, never with `GLANCE_DATA_DIR`).
  - `files.rs`: send (attach/drag & drop), offers, downloads with progress into Downloads (`GLANCE_DOWNLOAD_DIR` overrides), inline image previews.
  - `popout.rs`: fullscreen (immersive) viewing and pop-out windows (a popped-out stream gets its own Watch/feed; closing the window brings it back). Pop-outs are client-decorated with no top bar: drag the video to move, double-click toggles fullscreen, the floating controls auto-hide after 2 s (`AutoHide`); every close path goes through `GlanceApp::popout_closed`.
  - `dms.rs`: direct messages (conversation view, outbox resent when a contact comes Online, unread badges, notifications, writer thread to `dms/`). `direct.rs`: files sent to a contact outside a call.
  - `recent.rs`: recent rooms on the home screen (Rejoin / Start again / Forget), room liveness checks, saving and merging room chat.
  - `update/` (`release.rs` pure: install kind, asset choice, semver, minisign; `mod.rs` check/download/stage; `apply.rs` per-kind replace and relaunch; `cli.rs` `glance update`) and `update_ui.rs` (6-hour timer, title bar pill, Settings → Updates).
  - `markdown_view.rs`: renders a parsed chat message (room chat and DMs) with `RichText` plus plain divs for quote bars, list markers, code boxes and block spoilers; `MarkdownCache` (parsed messages by text), spoiler reveals (`revealed_spoilers`, per session), link clicks (invites join, web links go through the "Leaving glance" dialog in `overlays.rs` unless the domain is in `trusted_domains` in settings.json).
  - `rich_text.rs`: `RichText`, selectable styled text with clickable ranges, tooltips, a hover style and `masked_ranges` (hidden spoilers copy as a mask).
  - `code_highlight.rs`: code-block highlighting (syntect + two-face grammars folded into `SyntaxKind`, palette colours); a grammar's first parse runs on a worker and `set_on_ready` triggers a re-render.
  - `composer.rs`: the chat box for room chat and DMs (`new_composer`, `composer_element[_with_paste]`) with live markdown highlighting from `markdown::composer_spans`.
  - `camera.rs`: webcam on/off (camera track broadcast at 640×360@24) and auto-watching everyone's camera into person tiles.
  - `ui.rs`: shared widgets (Avatar, icon_button, pill, live_badge).
  - `video.rs`: `VideoView` tile. `spike.rs`: the render benchmark.
  - `theme.rs` + `assets/themes/glance.json`: design tokens and the gpui-kit component theme (dark + light).
- `crates/glance-core`: view-model types the UI renders (`model.rs`), the persistent contact book with its state transitions (`contacts.rs`), the Discord-style chat markdown parser (`markdown.rs`: `parse` into blocks and styled spans, `plain_text` for previews and notifications, `composer_spans` for the composer), the DM store (`dms.rs`: `dms/`, 2000 messages per contact; damaged files are kept as `.corrupt-<secs>` and the index is rebuilt), recent rooms and saved room chat (`rooms.rs`: rooms.json and `chats/<room id>.json`), `voice.rs` (`SelfVoice` for mute/deafen, `PeerAudio` prefs), formatting helpers, `demo.rs` fake data. Room/contacts state machines land here (no I/O, unit-tested).
- `crates/glance-net`: `Node` (iroh endpoint + router) and the room mesh actor (`room.rs`): membership, chat, state, voice as QUIC datagrams straight to an audio sink, chat/file history for late joiners, and video fan-out. `contacts.rs`: always-on contacts actor (ALPN `glance/contacts/2`, `/1` still accepted): requests, presence, ring/answer, join requests, DMs with an automatic ack; blocked peers refused, non-contacts can only send requests. `files.rs`: iroh-blobs store (on disk in the data dir), add/download/export, `unique_path` sanitizes peer-supplied names. (one QUIC uni stream per watcher, per-watcher queue that skips to keyframes when full). `probe.rs`: room liveness probes (ALPN `glance/room-probe/1`) behind `Node::join_room_via` / `host_room` / `restart_room`. `tests/room_mesh.rs` runs in-process peers; `tests/rejoin.rs`, `tests/contacts.rs` and `tests/dms.rs` cover rejoining, contacts and DMs.
- `crates/glance-media`: voice (`audio/`: cpal devices via lock-free rings, speexdsp echo cancellation + RNNoise suppression, Opus with FEC/DTX, jitter buffer, mixer/engine thread; `synth.rs` is the modal pluck + Freeverb synthesizer behind every sound, `ringtone.rs` the incoming ringtone and outgoing ringback, `sounds.rs` the chimes; no audio files are shipped), webcam capture via nokhwa (`camera.rs`), frames and scaling (`frame.rs`), H.264 (`codec.rs`: `VideoEncoder` picks a hardware encoder from `hw/` (NVENC via run-time loaded driver + vendored bindings in `hw/nvenc_sys.rs`; VideoToolbox via hand-written FFI in `hw/videotoolbox.rs`, which only CI compiles; VA-API in `hw/vaapi.rs` with packed headers from `hw/h264_headers.rs`, tried after NVENC) or falls back to OpenH264; decoding is always OpenH264), capture (`capture/`: Wayland = portal + PipeWire in `wayland.rs`; X11/macOS/Windows = zed-scap in `scap_backend.rs`), and `pipeline.rs` (broadcast: capture -> scale -> encode only while watched; decoder thread for viewers).
- `crates/glance-proto`: tickets (`glance://join/...`), room control messages and join proofs, length-prefixed postcard framing. Breaking wire changes get a new ALPN.
- `scripts/`: host build helpers, `shot.sh` (screenshots) and the packaging scripts (`package-appimage.sh`, `package-macos.sh`, `flatpak-cargo-sources.sh`).
- `packaging/`: `linux/` desktop entry and metainfo (app id `io.github.owallzz.glance`), `flatpak/` manifest, `windows/` `glance.iss` and `glance.ico` (embedded by `build.rs`), `macos/` Info.plist, and `minisign.pub` (the release signing public key the updater trusts).
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
- Git identity for this repo is set locally to the GitHub noreply address; don't change it.
- CI runs on GitHub-hosted runners only (`ubuntu-latest`, `macos-latest`, `windows-latest`); the repo is public, so never point a workflow at a self-hosted runner. Linux system packages go in the `apt-get install` line of each Linux job. Caching uses `Swatinem/rust-cache`. Keep `permissions: contents: read` at the top of `ci.yml`, and never run fork PR code with secrets (`pull_request_target`).
- Project skills live in `.claude/skills/`. Use them, and add one when a multi-step task keeps coming up.

## Releasing
1. Bump `version` in the root `Cargo.toml` (`[workspace.package]`) and add a `<release>` entry to `packaging/linux/io.github.owallzz.glance.metainfo.xml`, run `cargo build` to refresh `Cargo.lock`, commit, push, and wait for CI to pass on all three platforms.
2. Optional: run `release.yml` with `workflow_dispatch` to test-build every package (nothing is published or signed without a tag).
3. `git tag -a vX.Y.Z -m "glance vX.Y.Z" && git push origin vX.Y.Z`. `release.yml` checks the tag against the version; each platform job only builds artifacts, then the `sign` job signs every asset with minisign (secret `MINISIGN_SECRET_KEY`; tagged releases fail without it) and publishes all assets plus their `.minisig` files in one go. Assets: `glance-vX.Y.Z-linux-x86_64`, `.AppImage`, `.flatpak`, `glance-vX.Y.Z-windows-x86_64.exe`, `-windows-x86_64-setup.exe`, `-windows-x86_64-portable.zip`, `glance-vX.Y.Z-macos-aarch64`, `.dmg`, `.app.tar.gz`.
4. Write release notes with `gh release edit vX.Y.Z --notes-file ...`: what works, what doesn't, and the unsigned-build workarounds.

The public key is `packaging/minisign.pub`. The private key lives only on the maintainer's machine and in the repo secret: never commit it.

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
- GUI tests can run on a private Xvfb display so they never steal the user's focus: `Xvfb :99 -screen 0 1600x1000x24 -nolisten tcp &`, then `DISPLAY=:99 WAYLAND_DISPLAY= VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json ./target/debug/glance`, driven with `DISPLAY=:99 xdotool` (absolute coordinates = coordinates in an `import -window root` screenshot). Without a window manager, windows get server-side decorations (pop-out window buttons hidden).
- Keep GUI test audio off the user's speakers and mic: create a null sink (`support.null-audio-sink`, e.g. `node.name=glance-test-sink`, see the audio share probe row) plus one with `media.class=Audio/Source` for the mic, and set `input_device`/`output_device` in the test data dir's settings.json to their descriptions. The bots' 440 Hz stream tone is audible otherwise.
- Contacts protocol is `glance/contacts/2` (DMs); `/1` is still accepted and offered so 0.1.0 peers keep presence, rings and files. Only append `ContactMsg` variants, and never send DM frames to a peer whose connection isn't `/2`. The app owns the DM outbox: the actor doesn't queue DMs.
- `Dms::save` only marks a conversation as changed; call `Dms::flush` after a batch of changes (the contacts event loop does this once per burst).
- Room chat is deduplicated by (author, sent_at, content): send your own messages with `Command::Post` using the same `sent_at` you show locally. `History` arrives before the `MemberJoined` of whoever welcomed you, so tests should wait for it first.
- Rejoining and joining both dial whoever is listed (`entry`); the smaller-id rule only applies between existing members. `RoomStatus::Empty` means every candidate answered `NotHere`: never host a remembered room on a partial answer, use `Node::restart_room`, which checks again and joins anyone it finds. `host_room` makes us the ticket's host.
- The home screen's room check runs only when the list comes on screen, after leaving a room, or on refresh (at most every 30 s, at most 4 members per room). Don't start it from render timers.
- Local mute is never broadcast. Stream audio has its own engine key (`stream_audio::stream_key`), so its own volume and mute; deafen silences both. Saved audio choices must be in the engine before `SetAudioSink`, because voice data skips the UI.
- A live stream changes quality and source through `BroadcastControl::reconfigure` and `replace_capture`. Encoders are rebuilt at most once a second (each rebuild is a keyframe with SPS/PPS); frames are scaled to the current encoder until then. `Capture::set_fps` changes the rate live (zed-scap rebuilds its capturer on the same source). Stream settings opens on the running source even when a fresh source list misses it, so Apply never switches source unless the user picks one.
- Extra windows need `window_decorations: Some(WindowDecorations::Client)` and their own controls, or GNOME Wayland gives them no title bar (gpui-kit's `open_window` draws the Linux frame and edge resizing). On Windows, dragging needs `window_control_area(Drag)` (no `start_window_move`); buttons on top of a drag area need `.occlude()`.
- `on_hover` on a parent turns false while the pointer is over an `.occlude()` child, during any gpui drag, and after key presses. For "pointer left the window" listen to the raw `MouseExitEvent` (`popout::window_exit_watcher`). gpui has no public way to hide the cursor; see `popout::hide_cursor`.
- Release assets must keep the `glance-<tag>-<os>-<arch>[suffix]` names, and each needs `<name>.minisig` whose trusted comment contains `file:<name>`. A newer release without this platform's file or signature shows as NotPublished and is retried with the short backoff.
- An update restart passes `--after-update`, which waits for the old instance to exit before the single-instance hand-off.
- `$APPIMAGE` and `$APPDIR` are inherited by anything an AppImage launches. Start-on-login and link-handler entries use `net::launch_path()`, which is the `.AppImage` only when the executable is inside `$APPDIR`; start-on-login entries are rewritten on every launch while the setting is on.
- Debug binaries are over 1 GB, above the updater's 512 MB limit; strip them for manual updater tests.
- A `portable` file next to the exe or AppImage moves the data dir to `./data` and turns off autostart and scheme registration.
- Inside Flatpak: no self-update, autostart goes through the Background portal (a refusal switches the setting off), the tray runs without a well-known D-Bus name, and scheme registration is skipped.
- Regenerate `packaging/windows/glance.ico` with `glance --export-icons <dir>` when the icon changes; a test enforces it. The installer's AppId GUID in `glance.iss` must never change.
- The composer is gpui-kit's code-editor input (`EditorState`) with every code feature off and a custom `InputHighlighter`: the only input kind with per-range styling. It can't change font size or family per range, has no auto-grow (`composer_element` sizes it from the wrapped row count, one frame late), and Tab moves focus.
- `RichText` clicks need the press and release in separate frames (gpui's `InteractiveText` registers the mouse-up listener on the frame after the press): `xdotool click` is too fast, use `mousedown`, a short sleep, then `mouseup`.
- Chat text is rendered from the raw markdown every frame through `MarkdownCache`; never store rendered text (saved chat dedupes by raw text). Room chat is capped at `MAX_CHAT_LEN` (4000 bytes) like DMs.
- Test modules in files that `use gpui_kit::*` must import items explicitly, not `use super::*`: the glob brings GPUI's `test` macro, which shadows `#[test]` ("recursion limit reached").
