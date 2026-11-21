# glance: peer-to-peer screen sharing rooms with a Discord-style UI (Plan)

## Decisions (locked)
- **Name:** `glance`
- **Stack:** Rust (edition 2024), Cargo workspace.
  - UI: **gpui-kit 0.10** (Longbridge; gpui + gpui-component, from crates.io).
  - P2P: **iroh 1.x** (QUIC, hole punching, encrypted, peers addressed by public key, relay fallback).
  - Media: H.264 (openh264 software, hardware encoders added later), Opus audio, cpal, WebRTC audio processing (echo cancel / noise suppression).
  - Why: native and GPU-rendered, so it starts fast and stays light. No Electron, no WebRTC stack, and no server of our own.
- **Platform:** desktop Linux (Wayland + X11), macOS 13+, Windows 10/11. Developed and tested first on Linux (Fedora, this machine).
- **License:** none (all rights reserved). No LICENSE file.
- **Repo:** `owallzz/glance`, private.
- **CI:** fmt + clippy + test on each push. Linux runs on the owallzz self-hosted runner (`fedora:44` container); macOS (Apple Silicon) and Windows (x64) run clippy + test + release build on Blacksmith runners.
- **Releases:** tagged GitHub Releases with Linux x86_64 (self-hosted runner), macOS aarch64 and Windows x86_64 (Blacksmith) binaries, uploaded uncompressed. Installers (AppImage/.dmg/.msi) and signing come in M11.
- **MVP feature set:** multi-person rooms, everyone can share at once, voice chat, shared system/app audio, text chat, webcam, **contacts**, **file sharing**.
- **Resolution:** streams go above 1080p: 1440p, 4K (3840×2160), ultrawide (e.g. 5120×1440) and Source, at up to 60 fps (144 fps stretch). Above 1080p60 needs a hardware encoder (software H.264 can't keep up). The UI scales frames to each tile's on-screen size (see M1 results).
- **File sharing:** drop, paste or attach files in room chat, or send to a contact. Transfers are peer-to-peer via `iroh-blobs` (BLAKE3-verified, resumable, no size limit), and anyone who downloaded a file can serve it to others in the room.
- **Contacts:**
  - Added by a **mutual request**, offered after a call or from the participant list.
  - Invites **ring** with Join/Decline, and unanswered rings become missed calls.
  - **Presence** shows Online / Away / Do not disturb / Invisible plus "in a call", with an optional one-click Join.
  - glance stays reachable from a **tray icon** and **starts on login**. Both are toggles in settings and on by default.
  - Your contact identity is your persisted iroh key. There's no account server.
- **Design:** copies Discord's *design system*: dark palette, layout, spacing, radii, component shapes, the voice-channel and "Go Live" flows. It does **not** use Discord's name, logo, Clyde, sounds, or proprietary font. gg sans is proprietary, so we bundle **Inter** (OFL) in its place. glance gets its own logo and name, which avoids trademark and trade-dress trouble.

glance is a small native app for sharing screens with friends. You start a room, a link is copied to your clipboard, and you paste it to a friend. They join, and anyone in the room can Go Live while the others click Watch. There are no accounts, and media goes directly between peers. After the first call you can add each other as contacts. From then on, starting a call is one click on their name, and it rings them.

---

## 1. Target environment check

| Constraint | Reality | What it means for us |
|---|---|---|
| Linux Wayland capture | Apps can't read the screen directly. They must use the xdg-desktop-portal ScreenCast API, which shows the OS picker and then delivers frames over PipeWire. | Use `ashpd` + `pipewire`. On Wayland the OS picker replaces our thumbnail picker. Use X11 via XShm/xcap when `DISPLAY`-only. |
| Linux system audio | The portal gives video only. | Capture the PipeWire monitor of the default sink. Per-app audio by PipeWire node is stretch. Never loop back our own voice playback. |
| macOS | ScreenCaptureKit (12.3+, audio 13+). Needs the Screen Recording permission. Unsigned builds trip Gatekeeper. | Use the `screencapturekit` crate, which provides video and app audio. Hardware H.264 via VideoToolbox. Signing and notarization are needed for distribution. |
| Windows | Windows.Graphics.Capture for video, WASAPI loopback for audio, Media Foundation for hardware H.264. | Use the `windows-capture` crate and `wasapi`. We have no Windows machine here, so builds get checked by cross-compile only (risk). |
| P2P connectivity | iroh hole-punches most NATs. Symmetric NAT falls back to n0's public relays, which are slower and fair-use. | Works out of the box. Settings get a "custom relay URL" option (self-hosted `iroh-relay`). Show "Direct / Relayed" in the connection indicator. |
| Topology | There is no SFU, so each sharer uploads one copy per viewer (full mesh). 1080p60 is about 6 Mbps, so 4 viewers need about 24 Mbps up. | Encode **once** and fan out the same packets. Encode only while at least one viewer is watching (Discord behaviour). Adapt to the slowest viewer by dropping to the next keyframe. Target 2–8 people, with a soft warning above 8. |
| Rendering video in gpui | gpui renders `RenderImage` (BGRA) and has a `surface` element (macOS CVPixelBuffer). | Spike in M1: swap a BGRA `RenderImage` per frame and measure 1080p60 CPU and GPU cost. Use `surface` on macOS. If needed, fall back to a custom wgpu texture element. |
| Reaching contacts | iroh can dial a peer by public key alone: it looks up current addresses via n0's DNS/pkarr discovery, plus mDNS on the LAN. The peer must be running. | Contacts store only the EndpointId. glance runs in the tray, starts on login, and keeps no capture, audio, or encode running there. |
| Tray icons | `tray-icon` works on macOS and Windows. Linux uses StatusNotifierItem, which stock GNOME only shows with the AppIndicator extension (most distros ship it). | If no tray host exists, closing the window minimises instead of hiding, and we say so in settings. |
| Notifications / autostart | `notify-rust` (Linux/macOS), Windows toasts, and `auto-launch` (XDG autostart, LaunchAgent, Run key). Flatpak/portal autostart is a later concern. | Incoming rings use an OS notification plus our own popup window and ringtone. |
| CI runners | Self-hosted: Linux x86_64, Fedora container. Blacksmith: macOS 26 Apple Silicon, Windows Server 2025 x64. | Linux CI installs `-devel` packages in the container; macOS/Windows build natively on Blacksmith. |

Consequences:
- Capture, audio I/O, and hardware codecs go behind traits (`Capturer`, `AudioSource`, `VideoEncoder`/`Decoder`) with a per-OS backend.
- The protocol and room logic are pure Rust with no GUI, so they're unit-testable and run headless in CI.
- A headless `glance bot` mode joins a room and streams a test pattern plus a tone. This lets one machine test 3+ peers.

## 2. Scope

**v0.1 does:**
- **Rooms:** "Start a room" gives a link (`glance://join/<ticket>`) plus a copyable code, auto-copied. Others join by link click (URL scheme), by pasting, or by the Join box. There's a display name and avatar colour on first run and no accounts. The identity key is persisted so friends recognise you.
- **Screen sharing:** anyone can Go Live, and many at once. The picker offers screens and windows (thumbnails on X11/macOS/Windows, OS picker on Wayland). Quality presets: 720p30, 1080p60, 1440p60, 4K30, 4K60, Source (native resolution and refresh, ultrawide included). Presets the machine's encoder can't sustain are shown disabled with a reason. There's an optional "share audio" toggle.
- **Watching:** a LIVE badge in the participant list and a "Watch stream" tile. Views:
  - grid view, focus view with filmstrip, double-click to focus, fullscreen
  - pop-out stream window
  - per-stream volume, stop watching
  - stream stats overlay (resolution, fps, bitrate, direct/relayed, latency)
- **Voice:** mic, mute/deafen, input/output device pick, input sensitivity / VAD, noise suppression and echo cancel, a green speaking ring, per-user volume, join/leave sounds (our own).
- **Text chat:** a room chat panel with ephemeral history in the session and unread badge.
- **File sharing:**
  - **Sending:**
    - Drag files or folders onto the room window, paste from the clipboard, or click the attach button in chat. A file card (name, size, type icon) is posted to everyone in the room.
    - From the contacts home, "Send file" sends to one contact, who gets a notification. This works without a call when both apps are online.
  - **Receiving:**
    - Images and short videos under 25 MB preview inline automatically.
    - Other files need a click on Download (or Save as…), and save to the Downloads folder by default.
    - The file card shows a progress bar, speed, and ETA. Received files are hash-verified.
    - Executable types get an "Are you sure?" prompt before opening.
  - **Serving:** files are served from the sender's disk while they're online. Nothing is uploaded until someone downloads. Each receiver also serves its copy to the rest of the room, so the load spreads.
  - Interrupted transfers resume. Cancel and retry are available. Transfers run at lower priority than voice and video, so they don't wreck a call.
- **Webcam:** a camera toggle that shows camera tiles alongside screen tiles, with a device pick.
- **Contacts:** a Discord-style home screen with a contacts list (tabs: Online / All / Pending / Blocked) and recent calls.
  - **Adding:**
    - The end-of-call screen lists the people you called, each with an "Add contact" button. There's also "Add contact" in a participant's right-click menu.
    - The other person gets a request (Accept/Ignore). Only mutual contacts are saved.
    - Requests to someone offline are retried when you next see them.
  - **Calling:** click a contact (or multi-select) and press "Call". This creates a room and rings everyone selected. "Invite" in a room opens a contact picker and rings them into the current room.
  - **Ringing:**
    - An incoming-call popup (caller avatar, who else is in the room, Join / Decline) plus an OS notification and ringtone.
    - A ring times out after 30 s and becomes a missed call with a badge and a "Call back" button.
    - Do not disturb blocks rings and turns them into silent missed calls.
  - **Presence:**
    - Status is Online / Away (auto after idle) / Do not disturb / Invisible, plus "In a call (3)".
    - With "Let contacts join my calls" on, which defaults to off, contacts see a Join button. The ticket is requested on demand and is never broadcast.
  - **Manage:** nickname, favorite (pinned to top), remove, block (rejects connections from that key). Contacts are stored locally in `contacts.json` in the data dir, written atomically.
- **Background:** a tray icon with a menu (Open, status picker, Quit), start on login, and closing the window hides to the tray. In the tray glance only keeps contact connections open, and the window and UI state are dropped.
- **Settings:** audio, video, stream quality defaults, relay URL, appearance (dark and light), and in-app keybinds (mute, deafen). Persisted as TOML in the OS config dir.
- Light and snappy: target <150 ms cold start, 0% CPU and <100 MB proportional memory (PSS) idle, and no work done when nobody watches. M1 measured 0% CPU and about 49 MB private memory idle. Most of the 179 MB RSS is shared GPU driver libraries.

**Out of scope for now:**
- Accounts, servers/guilds, persistent chat history, DMs/messaging contacts outside a call (sending a file to a contact is the one exception).
- Offline file delivery: the sender must be online, or someone else who has the file. There's no cloud storage.
- Codecs beyond H.264 (so 8K and HDR are out for now).
- Syncing contacts across your own devices (each install is its own identity for now). Contact discovery by username or search, since adding needs a shared call or link.
- An SFU/relay media server, and recording.
- Remote control of a shared screen, and drawing/annotation.
- Global push-to-talk hotkey. This is hard on Wayland, so it comes later behind a portal (GlobalShortcuts).
- Simulcast (multiple qualities per sharer), AV1/VP9 codecs, mobile, and web.

## 3. Architecture

```
 ┌─────────────── glance-app (gpui-kit UI) ───────────────┐
 │ views: Home · Room(sidebar, stage, control bar, chat)  │
 │ modals: Go Live picker · Settings · Join               │
 │ theme: Discord-style tokens → gpui-kit Theme           │
 └───────▲──────────────────────────────┬─────────────────┘
         │ RoomEvent (state snapshots,  │ Commands (share, watch,
         │ decoded frames as Arc<Frame>)│ mute, chat, ...)
 ┌───────┴──────────── glance-core ─────▼─────────────────┐
 │ Room state machine (pure, tested) · session controller │
 └───────▲──────────────────────────────┬─────────────────┘
 ┌───────┴──── glance-net ────┐  ┌──────▼──── glance-media ─────────┐
 │ iroh Endpoint, ALPN        │  │ capture (portal/PipeWire, X11,   │
 │ "glance/1"; full-mesh      │  │ SCK, WGC) · webcam (nokhwa)      │
 │ room: control stream per   │  │ video enc/dec (openh264; HW: VA- │
 │ peer, uni-stream per video │  │ API/NVENC, VideoToolbox, MF)     │
 │ GOP, datagrams for Opus,   │  │ audio: cpal, opus, AEC/NS/VAD,   │
 │ gossip of member list      │  │ jitter buffer, mixer, loopback   │
 └───────────▲────────────────┘  └──────────────────────────────────┘
             └── glance-proto: wire types (postcard), framing, tickets
```

- **Room membership:** the ticket contains the creator's EndpointId and addrs plus a random room secret. On join you connect to any member, which sends the member list, and you dial everyone (mesh). The room secret is checked on handshake, so a ticket is the invite.
- **Contacts:**
  - A second ALPN `glance-contacts/1` handles contact requests, presence, rings, and join-ticket requests. Each app keeps a lightweight QUIC connection to each online mutual contact. It reconnects with backoff, so idle cost is keepalives only.
  - Every message is checked against the authenticated remote EndpointId, so a ring can only come from a saved contact and blocked keys are dropped at accept.
  - The contact store and presence logic live in `glance-core` as a pure state machine with tests. The network side lives in `glance-net/contacts`.
- **Files:**
  - Each app runs an `iroh-blobs` store (`glance-net/files`) on a third ALPN, with an on-disk store in the data dir and imports done by reference so big files aren't copied.
  - A file offer is a chat message carrying the blob hash, name, size, MIME type, and the provider list. Downloads fetch from any peer in the room that has the file.
  - Partial downloads persist, so they resume after restart.
- **Media flow:** the sharer captures, converts BGRA to I420 on the capture thread, encodes once, and fans packets out to each subscribed viewer. Each viewer has a bounded queue. If a queue overflows, it drops until the next keyframe and requests one (PLI-like). On the viewer side, a decode thread produces BGRA `Arc<Frame>` and the UI swaps the image and requests a repaint only for visible tiles.
- **Audio:** 20 ms Opus frames go out as QUIC datagrams, through a per-peer jitter buffer and then a mixer to the output device. Shared-screen audio is a separate Opus track tied to that stream's volume slider.
- **Threads:** a tokio runtime for net, dedicated threads for capture/encode/decode/audio, and the gpui main thread for UI. Channels are bounded crossbeam/tokio channels.

```
glance/
  Cargo.toml            # workspace
  crates/
    glance-proto/       # messages, framing, ticket encode/decode (+ tests)
    glance-net/         # iroh endpoint, room mesh, streams, contacts, files (+ in-process 2–3 peer tests)
    glance-media/       # capture/, video/, audio/, webcam/ with per-OS backends
    glance-core/        # room + contacts state machines, stores, session controller (+ tests)
    glance-app/         # gpui-kit UI, tray, notifications, binary `glance` (also `glance bot`)
      assets/           # Inter font, glance icons/logo, sounds (our own)
  packaging/            # .desktop + URL scheme, AppImage, macOS Info.plist, Windows manifest
  .github/workflows/    # ci.yml, release.yml
```

## 4. Milestones
Each milestone ends with something that runs. Commit and push in small steps.

- [x] **M0: Scaffold.** Workspace, gpui-kit window with Discord-style theme tokens and Inter, CI green, README/AGENTS.md.
- [x] **M1: Video render spike.** Test-pattern tiles at up to 4K60, measured.
  - **Result:** per-frame `RenderImage` swap plus `drop_image` works.
    - Uploading full 4K every frame costs about 65% of the UI thread (~50 fps).
    - Fix: tiles publish their on-screen pixel size and producers scale frames to it.
    - After the fix, 4 × 4K60 and 9 × 1440p60 all paint at 60 fps with the UI thread under 8%.
    - Native Wayland is required: XWayland throttles to about 1 fps in the background.
  - **Open:** fullscreen 4K on a 4K monitor still uploads full frames. If that's too heavy, patch `gpui-pre-wgpu` with a persistent texture plus YUV upload (see §6).
- [x] **M2: UI shell with fake data.** Home (Start/Join), room view (participant sidebar with avatars, speaking rings, and LIVE badges), stage grid/focus/filmstrip, control bar, user panel, chat panel, Go Live modal, settings dialog. Fully clickable.
- [x] **M3: Rooms over iroh.** Create/join by ticket, mesh membership, presence, text chat, connection indicator. Two or three instances on one machine, plus `glance bot`.
- [x] **M3b: Contacts + ringing.** Persisted identity, end-of-call "Add contact", mutual requests, contacts home (Online/All/Pending/Blocked), presence/status, Call and Invite rings with incoming-call popup, missed calls, block/remove, tray + start on login + OS notifications. Tested with two local instances using separate data dirs.
- [x] **M3c: File sharing.** iroh-blobs store, drag/drop, paste and attach in chat, file cards with progress, inline image preview, multi-provider download, resume, send-to-contact.
- [x] **M4: First real screen share.** Portal+PipeWire (Wayland), zed-scap (X11/macOS/Windows), OpenH264 encode/decode, one sharer to many watchers, Watch/Stop watching, encode-only-when-watched, own-stream preview.
  - Measured on Linux (release build): real 2560×1440 capture at about 52 fps with moving content. Software encode at 1080p takes 18–25 ms per frame. The viewer gets a steady 30 fps from a 1080p30 stream at about 22% of one core.
  - Not yet checked on real macOS/Windows hardware (CI builds and tests only).
- [x] **M5: Everyone shares at once.** Multiple simultaneous streams, keyframe requests and per-viewer backpressure (done in M4), plus bitrate adaptation, stats overlay, fullscreen and pop-out.
- [x] **M6: Voice.** cpal + Opus + AEC/NS/VAD, jitter buffer, mixer, mute/deafen, speaking ring, per-user volume, device selection, sounds.
- [x] **M7: Stream audio.** System/app audio with screen shares, per-stream volume.
  - Linux: PipeWire capture of one app's output, or "Entire system", which links every other app's output into our capture stream and skips glance's own nodes (matched through their PipeWire client's process id), so viewers never hear the call echoed back. Windows: WASAPI process loopback of everything except glance's own process tree (Windows 10 2004+; type-checked against the Windows target and compiled in CI, not yet run), falling back to device loopback, which includes the call, on older Windows. macOS: ScreenCaptureKit system audio excluding glance itself (compiles in CI; not yet run on a Mac).
  - Opus at 64 kbps (music mode), sent as datagrams only to people watching the screen; receivers mix it as a separate source with its own volume.
- [x] **M8: Webcam.** Camera tiles via nokhwa, sharing the video pipeline.
- [ ] **M9: macOS + Windows backends.** Done so far: macOS .app bundle + .dmg in releases (usage strings, URL scheme, icon; ad-hoc signed), single instance with link hand-off, `glance://` registration on Linux/Windows, macOS open-URL events. SCK/WGC capture plus audio, VideoToolbox/Media Foundation encode/decode, URL scheme registration, permissions prompts.
- [ ] **M10: Hardware codecs on Linux + quality presets.**
  - OpenH264 encodes in parallel slices (our own wrapper; the crate forced one slice): 1440p60 at a full 60 fps in software, 4K about 40 fps.
  - **NVENC** (Linux + Windows, `hw/nvenc.rs`): the driver is loaded at run time through vendored NVENC 12.0 bindings, with a CUDA context. BGRA goes in (the GPU converts it) and Constrained Baseline comes out, so viewers keep decoding with OpenH264. Measured on a recent NVIDIA RTX card: 4K in 11.6 ms per frame, and a 4K60 stream decoded at 60 fps between two local peers.
  - If the GPU encoder fails mid-stream, glance falls back to software. There's a Settings → Streaming toggle, and the Go Live warning only shows without a GPU encoder.
  - Viewers scale the Y/U/V planes to the tile and convert to BGRA in parallel (rayon): 4K into a 1080p tile takes about 13 ms (OpenH264 decode is about 9.5 ms of that).
  - Streams fit within 4096x2304 (H.264 level 5.2, and the most GPU encoders take) and 4K's pixel count, so 5120x1440 goes out as 4096x1152. Going wider needs HEVC or AV1.
  - **VideoToolbox** (macOS, `hw/videotoolbox.rs`): a hardware session with low-latency rate control, Constrained Baseline, and AVCC converted to Annex-B with SPS/PPS before keyframes. Tested in CI with Apple's software encoder; not yet run on a real Mac.
  - **VA-API** (Linux AMD/Intel, `hw/vaapi.rs`): libva is loaded at run time and glance tries each render node until one has a Constrained Baseline encoder. BGRA is converted to NV12 on the CPU in parallel. Mesa writes no headers itself, so `hw/h264_headers.rs` supplies SPS, PPS and slice headers as packed headers. On an AMD integrated GPU, 4K encodes in 15 ms per frame and the sharer holds 58 fps. NVENC is tried first.
  - **Media Foundation** (Windows AMD/Intel, `hw/mediafoundation.rs`): the vendor's H.264 encoder MFT (async events handled), fed NV12 from memory. mfplat.dll is loaded at run time, so Windows N and Server without Media Foundation still start. CI tests it with Microsoft's software MFT; not yet run on a real AMD/Intel Windows PC.
  - **NVDEC** (Linux + Windows, `hw/nvdec.rs`): the driver's parser and decoder, with the frame scaled to the tile on the GPU (the output size changes at the next keyframe) and copied back as NV12. 4K into a 1080p tile takes 5 ms instead of 14; a 4K60 viewer holds 60 fps at 74% of one core where OpenH264 manages about 34 fps at 196%. On failure it switches to OpenH264 and asks for a keyframe.
  - **VideoToolbox decode** (macOS, `hw/vtdecode.rs`): Annex-B is rewrapped as AVCC sample buffers for a VTDecompressionSession, which scales to the tile (remade at the next keyframe when the tile changes) and returns NV12. Tested in CI; not yet run on a real Mac.
  - Remaining: hardware decode on AMD/Intel (VA-API decode needs our own slice-header and reference-list handling; Media Foundation's decoder needs a D3D11 device to use the GPU).
- [ ] **M11: Polish + release.** Settings persistence, keybinds, light theme, notifications, AppImage/.dmg/.msi packaging, v0.1.0 tag.

## 5. Running and testing
- Run: `cargo run -p glance-app` (or `cargo run -p glance-app -- join <link>`, `-- bot <link>`)
- Test: `cargo test --workspace`
  - Proto round-trips, room state machine, jitter buffer, and ticket parsing.
  - In-process iroh tests (relay disabled, localhost) with 3 peers exchanging synthetic frames and chat.
  - Contact tests: mutual add, ring/decline/timeout, block, DND.
  - File tests: 3-peer transfer, resume after interruption, hash mismatch rejected, second downloader fetches from the first receiver.
- Multiple local identities: `GLANCE_DATA_DIR=/tmp/... cargo run -p glance-app` runs a second "person" on one machine.
- Lint: `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all --check`
- Manual end-to-end: run two app instances plus one or two `glance bot` peers locally, then a real test across two networks.
- CI: fmt, clippy, and test on each push. Release on `v*` tags builds the Linux tarball and AppImage.
- A project skill `run-glance` (launch N local peers and screenshot) gets added once M3 lands.

## 6. Risks and open questions
- **gpui-kit is pre-1.0** and pins exact `gpui-pre` versions, so expect API churn. Pin the version and wrap gpui-kit use in our own components.
- **Video in gpui:** if per-frame `RenderImage` swaps are too slow, write a custom element with a persistent GPU texture. M1 exists to find this out early.
- **Wayland portal quirks** vary by compositor (GNOME/KDE/Hyprland). Test on the local desktop first.
- **Fullscreen 4K upload:** gpui on Linux/Windows has no external-texture surface, so a full-size 4K frame is a new atlas texture plus three copies. Fallback: a small `[patch.crates-io]` fork of `gpui-pre-wgpu` adding a persistent video texture with YUV planes and shader conversion.
- **4K60 needs hardware encode:** without a supported GPU encoder (NVENC; VideoToolbox on macOS), 4K tops out around 40 fps in software. The Go Live picker warns about this.
- **Open (file sharing):** auto-preview limit (25 MB) and default save folder are guesses, so say if you want them different. Folder sharing is planned as a zipped/collection blob, and per-file approval for big files from non-contacts is still to decide.
- **Mesh bandwidth** caps practical room size for high-quality shares. Document it and warn in the UI. An optional SFU is a later project.
- **Contacts depend on both apps running** (tray + autostart) and on n0's discovery service. If discovery is down, contacts on another network can't be rung, but a pasted link still works.
- **Linux tray** needs a StatusNotifierItem host (GNOME needs the AppIndicator extension). There's a fallback, but it's less seamless.
- **Open: multi-device identity.** Each install is a separate contact for now. Linking devices (shared contact list, ring all your devices) is a later feature.
- **Windows is untested here.** Only cross-compile checks are possible without a machine or runner.
- **macOS/Windows CI** runs on Blacksmith (decided). Windows runners are Blacksmith public beta; macOS is Apple Silicon only, so Intel Mac builds would need cross-compiling.
- **Open: code signing.** macOS notarization needs an Apple Developer account, and Windows SmartScreen prefers a signing cert. Unsigned builds work with warnings.
