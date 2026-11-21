---
name: release
description: Publish a new glance release (version bump, tag, signed packages for Linux/Windows/macOS, release notes) and verify the result. Use when asked to release, publish, tag or ship a new version of glance.
---

# Publish a glance release

Releases are built and signed by `.github/workflows/release.yml` when a `vX.Y.Z` tag is pushed. Publishing is outward-facing and hard to undo (installed copies auto-update to it), so only do it when the user asked for a release.

## 1. Pick the version
- Check the latest release (`gh release list -L 3`) and what changed since its tag (`git log --oneline <last-tag>..HEAD`).
- Semver while 0.x: new features → bump minor (0.2.0 → 0.3.0), only fixes → bump patch. Ask the user if unsure.
- If the root `Cargo.toml` already has a version that was never tagged (check `git tag -l`), release that version instead of bumping again.

## 2. Bump (skip if already done)
1. `version` under `[workspace.package]` in the root `Cargo.toml`.
2. A new `<release version="X.Y.Z" date="YYYY-MM-DD">` entry at the top of `<releases>` in `packaging/linux/io.github.owallzz.glance.metainfo.xml` with a one-paragraph description. Validate: `appstreamcli validate --no-net --pedantic packaging/linux/io.github.owallzz.glance.metainfo.xml`.
3. `source scripts/env.sh && cargo build` to refresh `Cargo.lock`; `./target/debug/glance --version` prints the new version.
4. Commit ("Bump the version to X.Y.Z"), push.

## 3. Wait for green CI on the exact commit you'll tag
- `gh run list -L 3 --workflow ci.yml`; wait with `gh run watch <id> --exit-status --interval 120` (run it in the background). All three jobs (Linux, macOS, Windows) must pass. Runs on older commits get cancelled by newer pushes; that's fine, only the head commit's run counts.
- Run fmt, clippy and tests locally first if anything changed since the last green run.
- Optional for packaging changes: `gh workflow run release.yml --ref main` test-builds every package without publishing (signing runs too, since the secret is set; nothing is uploaded to a release without a tag).

## 4. Tag and push
```bash
git tag -a vX.Y.Z -m "glance vX.Y.Z"
git push origin vX.Y.Z
```
The `release` workflow checks the tag matches the workspace version, builds Linux (binary + AppImage), Flatpak, macOS (binary, .dmg, .app.tar.gz) and Windows (exe, setup.exe, portable.zip), then the `sign` job signs every asset with minisign (repo secret `MINISIGN_SECRET_KEY`) and publishes all assets plus `.minisig` files in one go. It takes ~20–30 min; watch it in the background with `gh run watch`.

If a job fails: fix on main, delete the tag locally and remotely (`git tag -d vX.Y.Z && git push origin :refs/tags/vX.Y.Z`), delete the draft/partial release if one was created (`gh release delete vX.Y.Z --yes`), and tag again once CI is green. Never re-tag a version that was already published with assets people may have installed: bump the patch version instead.

## 5. Verify the published release
- `gh release view vX.Y.Z --json assets -q '.assets[].name'` lists exactly these 18 files (9 assets + 9 `.minisig`):
  `glance-vX.Y.Z-linux-x86_64`, `-linux-x86_64.AppImage`, `-linux-x86_64.flatpak`, `-windows-x86_64.exe`, `-windows-x86_64-setup.exe`, `-windows-x86_64-portable.zip`, `-macos-aarch64`, `-macos-aarch64.dmg`, `-macos-aarch64.app.tar.gz`, each with `.minisig`. The updater matches these names exactly; a missing `.minisig` means that platform won't auto-update.
- Download into a fresh empty directory in the scratchpad and check signatures: `minisign -Vm <file> -p packaging/minisign.pub` for each asset (minisign is in Homebrew).
- Smoke-test Linux: `chmod +x` the AppImage and run `./glance-vX.Y.Z-linux-x86_64.AppImage --version` (add `--appimage-extract-and-run` without FUSE); the plain binary too.
- Check the updater sees it: a build from source is a "Development build" and never updates, so test with `GLANCE_UPDATE_URL=https://api.github.com GLANCE_UPDATE_CURRENT_VERSION=0.0.1 ./target/debug/glance update` (needs a debug build of the current tree; it reports the new version and that it would download the right asset).

## 6. Release notes
Write them to a file in the scratchpad and `gh release edit vX.Y.Z --notes-file <file>`. Structure:
- One-line summary, then **What's new** (user-facing features and fixes since the last release, from the git log; plain words, no internal names).
- **Install**: which file to pick per platform (table or list), linking to the README's Installation section; mention that existing installs (except the Flatpak bundle) update themselves, and that versions before 0.2.0 have no updater and need a manual download once.
- **First launch** on unsigned builds: Windows SmartScreen "More info → Run anyway"; macOS Gatekeeper (right-click → Open, or System Settings → Privacy & Security → Open Anyway, or `xattr -dr com.apple.quarantine glance.app`).
- **Known issues / not yet tested**: anything not verified on real hardware (e.g. macOS/Windows paths only CI has run).
- **Verify downloads**: `minisign -Vm <file> -p minisign.pub` with the key from `packaging/minisign.pub`.
Keep it free of personal info (no names, emails, machine paths).

## 7. Afterwards
- Tell the user the release URL and what was verified.
- If the release added features, make sure `README.md` and `PLAN.md` mention them (the README's "What's new" section should describe the latest version).
