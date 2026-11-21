//! The pure half of the updater: how glance was installed, which release
//! asset fits that install, whether a release is newer, and minisign
//! signature checks. No I/O here, so all of it is unit-tested.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use minisign_verify::{PublicKey, Signature};
use semver::Version;
use serde::Deserialize;

/// How this copy of glance was installed, which decides how it updates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallKind {
    /// Flatpak updates itself through flatpak; never self-update.
    Flatpak,
    /// Debug build, test identity or cargo target dir: never self-update.
    Dev,
    /// An AppImage at this path (`$APPIMAGE`): replace that file.
    AppImage(PathBuf),
    /// The Windows installer's per-user install: run the new setup.exe.
    WindowsInstalled(PathBuf),
    /// glance.exe with a `portable` marker beside it: replace the exe.
    WindowsPortable(PathBuf),
    /// The glance.app bundle at this path: replace the bundle.
    MacApp(PathBuf),
    /// A plain binary at this path: replace it.
    Binary(PathBuf),
}

impl InstallKind {
    pub fn label(&self) -> &'static str {
        match self {
            InstallKind::Flatpak => "Flatpak",
            InstallKind::Dev => "Development build",
            InstallKind::AppImage(_) => "AppImage",
            InstallKind::WindowsInstalled(_) => "Installed",
            InstallKind::WindowsPortable(_) => "Portable",
            InstallKind::MacApp(_) => "App bundle",
            InstallKind::Binary(_) => "Standalone binary",
        }
    }

    /// The file or bundle the update replaces (`None` when nothing is
    /// replaced in place).
    pub fn target(&self) -> Option<&Path> {
        match self {
            InstallKind::AppImage(p)
            | InstallKind::WindowsPortable(p)
            | InstallKind::MacApp(p)
            | InstallKind::Binary(p) => Some(p),
            InstallKind::Flatpak | InstallKind::Dev | InstallKind::WindowsInstalled(_) => None,
        }
    }
}

/// What the running process knows about itself, gathered by the caller.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    /// `std::env::consts::OS`.
    pub os: String,
    pub debug_build: bool,
    /// `GLANCE_DATA_DIR` is set (a test identity).
    pub data_dir_override: bool,
    /// `GLANCE_UPDATE_URL` is set: testing the updater, so a dev build may
    /// update itself.
    pub test_override: bool,
    /// `/.flatpak-info` exists or `FLATPAK_ID` is set.
    pub flatpak: bool,
    /// `$APPIMAGE`.
    pub appimage: Option<PathBuf>,
    /// `$APPDIR`, where the AppImage runtime mounted the image.
    pub appdir: Option<PathBuf>,
    /// `std::env::current_exe()`.
    pub exe: PathBuf,
    /// `%LOCALAPPDATA%` (Windows).
    pub local_app_data: Option<PathBuf>,
    /// A file named `portable` sits next to the exe.
    pub portable_marker: bool,
}

pub fn detect(f: &Facts) -> InstallKind {
    if f.flatpak {
        return InstallKind::Flatpak;
    }
    let dev = f.debug_build || f.data_dir_override || in_cargo_target(&f.exe);
    if dev && !f.test_override {
        return InstallKind::Dev;
    }
    // Only when we run from inside the mounted image: anything started by
    // another AppImage inherits that one's $APPIMAGE.
    if let (Some(path), Some(appdir)) = (&f.appimage, &f.appdir)
        && f.exe.starts_with(appdir)
    {
        return InstallKind::AppImage(path.clone());
    }
    if f.os == "macos"
        && let Some(bundle) = app_bundle(&f.exe)
    {
        return InstallKind::MacApp(bundle);
    }
    if f.os == "windows" {
        if f.portable_marker {
            return InstallKind::WindowsPortable(f.exe.clone());
        }
        if let Some(local) = &f.local_app_data
            && f.exe.starts_with(local.join("Programs").join("glance"))
        {
            return InstallKind::WindowsInstalled(f.exe.clone());
        }
    }
    InstallKind::Binary(f.exe.clone())
}

/// Whether `/.flatpak-info` says this Flatpak came from Flathub (which
/// updates it), rather than from a bundle file.
pub fn from_flathub(flatpak_info: &str) -> bool {
    flatpak_info
        .lines()
        .filter_map(|l| l.trim().strip_prefix("origin="))
        .any(|o| o.trim().eq_ignore_ascii_case("flathub"))
}

/// `.../target/debug/glance` or `.../target/<triple>/release/glance`.
fn in_cargo_target(exe: &Path) -> bool {
    let Some(profile_dir) = exe.parent() else {
        return false;
    };
    let profile = profile_dir.file_name().and_then(|n| n.to_str());
    matches!(profile, Some("debug" | "release"))
        && profile_dir
            .ancestors()
            .skip(1)
            .take(2)
            .any(|a| a.file_name().is_some_and(|n| n == "target"))
}

/// `X.app/Contents/MacOS/glance` -> `X.app`.
fn app_bundle(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    (macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && bundle.extension()? == "app")
        .then(|| bundle.to_path_buf())
}

/// The release asset for an install kind, per the release naming contract
/// (`glance-<tag>-<os>-<arch>[suffix]`). `None` for installs that don't
/// self-update.
pub fn asset_name(kind: &InstallKind, tag: &str, os: &str, arch: &str) -> Option<String> {
    let base = format!("glance-{tag}-{os}-{arch}");
    Some(match kind {
        InstallKind::Flatpak | InstallKind::Dev => return None,
        InstallKind::AppImage(_) => format!("{base}.AppImage"),
        InstallKind::WindowsInstalled(_) => format!("{base}-setup.exe"),
        InstallKind::WindowsPortable(_) => format!("{base}.exe"),
        InstallKind::MacApp(_) => format!("{base}.app.tar.gz"),
        InstallKind::Binary(_) if os == "windows" => format!("{base}.exe"),
        InstallKind::Binary(_) => base,
    })
}

/// The parts of a GitHub release we use.
#[derive(Clone, Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
}

pub fn parse_release(json: &str) -> Result<Release> {
    serde_json::from_str(json).context("unexpected release data from GitHub")
}

/// `v1.2.3` -> 1.2.3.
pub fn tag_version(tag: &str) -> Option<Version> {
    Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()
}

/// What to do about a release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    UpToDate,
    /// Newer, but this install kind has no build at all.
    NoAsset(Version),
    /// Newer, but the build for this install or its signature isn't on the
    /// release (yet: assets are uploaded one job at a time, signatures last).
    NotPublished(Version),
    Download {
        version: Version,
        asset: Asset,
        signature: Asset,
    },
}

pub fn plan(
    release: &Release,
    current: &Version,
    kind: &InstallKind,
    os: &str,
    arch: &str,
) -> Plan {
    let Some(version) = tag_version(&release.tag_name) else {
        return Plan::UpToDate;
    };
    if release.draft || release.prerelease || !version.pre.is_empty() || version <= *current {
        return Plan::UpToDate;
    }
    let Some(name) = asset_name(kind, &release.tag_name, os, arch) else {
        return Plan::NoAsset(version);
    };
    let find = |n: &str| release.assets.iter().find(|a| a.name == n).cloned();
    match (find(&name), find(&format!("{name}.minisig"))) {
        (Some(asset), Some(signature)) => Plan::Download {
            version,
            asset,
            signature,
        },
        _ => Plan::NotPublished(version),
    }
}

/// The embedded public key: a minisign `.pub` file (comment line plus key)
/// or a bare base64 key. Lines starting with `#` are ignored, so the
/// placeholder file parses to `None` (updates disabled).
pub fn parse_public_key(text: &str) -> Option<PublicKey> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("untrusted comment:"))
        .find_map(|l| PublicKey::from_base64(l).ok())
}

/// Decode a `.minisig` and check that it was made for `asset_name` (the
/// trusted comment names the file, which also names the version, so an old
/// signed build can't be passed off as a new release).
pub fn parse_signature(text: &str, asset_name: &str) -> Result<Signature> {
    let sig = Signature::decode(text).context("malformed signature file")?;
    let expected = format!("file:{asset_name}");
    if !sig
        .trusted_comment()
        .split_whitespace()
        .any(|t| t == expected)
    {
        bail!("signature was made for a different file");
    }
    Ok(sig)
}

/// Verify a whole file in memory (the downloader streams instead).
#[cfg(test)]
pub fn verify(key: &PublicKey, sig: &Signature, bytes: &[u8]) -> Result<()> {
    key.verify(bytes, sig, false)
        .map_err(|e| anyhow::anyhow!("bad signature: {e}"))
}

#[cfg(test)]
pub mod test_keys {
    //! A throwaway key pair made per test run (never the release key).
    use minisign::KeyPair;

    pub struct TestKey {
        pub kp: KeyPair,
    }

    impl TestKey {
        pub fn new() -> Self {
            Self {
                kp: KeyPair::generate_unencrypted_keypair().unwrap(),
            }
        }

        /// The key as a `.pub` file.
        pub fn public_file(&self) -> String {
            self.kp.pk.to_box().unwrap().to_string()
        }

        /// A `.minisig` for `data`, with the trusted comment the release
        /// job writes.
        pub fn sign(&self, data: &[u8], file_name: &str) -> String {
            let comment = format!("timestamp:1700000000\tfile:{file_name}\thashed");
            minisign::sign(Some(&self.kp.pk), &self.kp.sk, data, Some(&comment), None)
                .unwrap()
                .to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_keys::TestKey;
    use super::*;

    /// Trimmed from a real `releases/latest` response.
    const FIXTURE: &str = r#"{
      "url": "https://api.github.com/repos/owallzz/glance/releases/1",
      "html_url": "https://github.com/owallzz/glance/releases/tag/v0.2.0",
      "id": 1,
      "tag_name": "v0.2.0",
      "target_commitish": "main",
      "name": "glance v0.2.0",
      "draft": false,
      "prerelease": false,
      "created_at": "2026-11-20T10:00:00Z",
      "published_at": "2026-11-20T11:00:00Z",
      "assets": [
        {"name": "glance-v0.2.0-linux-x86_64", "size": 41000000, "content_type": "application/octet-stream",
         "browser_download_url": "https://github.com/owallzz/glance/releases/download/v0.2.0/glance-v0.2.0-linux-x86_64"},
        {"name": "glance-v0.2.0-linux-x86_64.minisig", "size": 300,
         "browser_download_url": "https://github.com/owallzz/glance/releases/download/v0.2.0/glance-v0.2.0-linux-x86_64.minisig"},
        {"name": "glance-v0.2.0-linux-x86_64.AppImage", "size": 45000000,
         "browser_download_url": "https://github.com/owallzz/glance/releases/download/v0.2.0/glance-v0.2.0-linux-x86_64.AppImage"},
        {"name": "glance-v0.2.0-linux-x86_64.AppImage.minisig", "size": 300,
         "browser_download_url": "https://github.com/owallzz/glance/releases/download/v0.2.0/glance-v0.2.0-linux-x86_64.AppImage.minisig"},
        {"name": "glance-v0.2.0-windows-x86_64.exe", "size": 40000000,
         "browser_download_url": "https://github.com/owallzz/glance/releases/download/v0.2.0/glance-v0.2.0-windows-x86_64.exe"},
        {"name": "glance-v0.2.0-windows-x86_64.exe.minisig", "size": 300,
         "browser_download_url": "https://github.com/owallzz/glance/releases/download/v0.2.0/glance-v0.2.0-windows-x86_64.exe.minisig"},
        {"name": "glance-v0.2.0-windows-x86_64-setup.exe", "size": 20000000,
         "browser_download_url": "https://github.com/owallzz/glance/releases/download/v0.2.0/glance-v0.2.0-windows-x86_64-setup.exe"},
        {"name": "glance-v0.2.0-windows-x86_64-setup.exe.minisig", "size": 300,
         "browser_download_url": "https://github.com/owallzz/glance/releases/download/v0.2.0/glance-v0.2.0-windows-x86_64-setup.exe.minisig"},
        {"name": "glance-v0.2.0-macos-aarch64.app.tar.gz", "size": 30000000,
         "browser_download_url": "https://github.com/owallzz/glance/releases/download/v0.2.0/glance-v0.2.0-macos-aarch64.app.tar.gz"},
        {"name": "glance-v0.2.0-macos-aarch64.app.tar.gz.minisig", "size": 300,
         "browser_download_url": "https://github.com/owallzz/glance/releases/download/v0.2.0/glance-v0.2.0-macos-aarch64.app.tar.gz.minisig"},
        {"name": "glance-v0.2.0-macos-aarch64", "size": 30000000,
         "browser_download_url": "https://github.com/owallzz/glance/releases/download/v0.2.0/glance-v0.2.0-macos-aarch64"}
      ],
      "body": "notes"
    }"#;

    fn facts(os: &str, exe: &str) -> Facts {
        Facts {
            os: os.into(),
            exe: exe.into(),
            ..Default::default()
        }
    }

    #[test]
    fn parses_the_release_fixture() {
        let r = parse_release(FIXTURE).unwrap();
        assert_eq!(r.tag_name, "v0.2.0");
        assert!(!r.draft && !r.prerelease);
        assert_eq!(r.assets.len(), 11);
        assert_eq!(r.assets[0].size, 41_000_000);
        assert!(parse_release("{\"message\":\"Not Found\"}").is_err());
    }

    #[test]
    fn compares_versions() {
        let r = parse_release(FIXTURE).unwrap();
        let kind = InstallKind::Binary("/opt/glance".into());
        let at = |v: &str| plan(&r, &Version::parse(v).unwrap(), &kind, "linux", "x86_64");
        assert!(
            matches!(at("0.1.0"), Plan::Download { ref version, .. } if version.to_string() == "0.2.0")
        );
        assert_eq!(at("0.2.0"), Plan::UpToDate);
        assert_eq!(at("0.10.0"), Plan::UpToDate, "semver, not string order");
        assert!(matches!(at("0.2.0-rc.1"), Plan::Download { .. }));
        let mut pre = r.clone();
        pre.prerelease = true;
        assert_eq!(
            plan(&pre, &Version::new(0, 1, 0), &kind, "linux", "x86_64"),
            Plan::UpToDate
        );
        pre.prerelease = false;
        pre.tag_name = "v0.3.0-beta.1".into();
        assert_eq!(
            plan(&pre, &Version::new(0, 1, 0), &kind, "linux", "x86_64"),
            Plan::UpToDate
        );
        pre.tag_name = "nightly".into();
        assert_eq!(
            plan(&pre, &Version::new(0, 1, 0), &kind, "linux", "x86_64"),
            Plan::UpToDate
        );
        let mut draft = r.clone();
        draft.draft = true;
        assert_eq!(
            plan(&draft, &Version::new(0, 1, 0), &kind, "linux", "x86_64"),
            Plan::UpToDate
        );
    }

    #[test]
    fn picks_the_asset_for_each_install_kind() {
        let r = parse_release(FIXTURE).unwrap();
        let v = Version::new(0, 1, 0);
        let pick = |kind: InstallKind, os: &str, arch: &str| match plan(&r, &v, &kind, os, arch) {
            Plan::Download {
                asset, signature, ..
            } => {
                assert_eq!(signature.name, format!("{}.minisig", asset.name));
                Some(asset.name)
            }
            _ => None,
        };
        let p = || PathBuf::from("/x");
        assert_eq!(
            pick(InstallKind::Binary(p()), "linux", "x86_64").as_deref(),
            Some("glance-v0.2.0-linux-x86_64")
        );
        assert_eq!(
            pick(InstallKind::AppImage(p()), "linux", "x86_64").as_deref(),
            Some("glance-v0.2.0-linux-x86_64.AppImage")
        );
        assert_eq!(
            pick(InstallKind::WindowsInstalled(p()), "windows", "x86_64").as_deref(),
            Some("glance-v0.2.0-windows-x86_64-setup.exe")
        );
        assert_eq!(
            pick(InstallKind::WindowsPortable(p()), "windows", "x86_64").as_deref(),
            Some("glance-v0.2.0-windows-x86_64.exe")
        );
        assert_eq!(
            pick(InstallKind::Binary(p()), "windows", "x86_64").as_deref(),
            Some("glance-v0.2.0-windows-x86_64.exe")
        );
        assert_eq!(
            pick(InstallKind::MacApp(p()), "macos", "aarch64").as_deref(),
            Some("glance-v0.2.0-macos-aarch64.app.tar.gz")
        );
        // The plain macOS binary has no signature in the fixture.
        assert_eq!(pick(InstallKind::Binary(p()), "macos", "aarch64"), None);
        assert_eq!(
            plan(&r, &v, &InstallKind::Binary(p()), "macos", "aarch64"),
            Plan::NotPublished(Version::new(0, 2, 0))
        );
        // No build for this platform.
        assert_eq!(pick(InstallKind::Binary(p()), "linux", "aarch64"), None);
        assert_eq!(
            plan(&r, &v, &InstallKind::Flatpak, "linux", "x86_64"),
            Plan::NoAsset(Version::new(0, 2, 0))
        );
        assert_eq!(
            asset_name(&InstallKind::Dev, "v1.0.0", "linux", "x86_64"),
            None
        );
    }

    #[test]
    fn detects_install_kinds() {
        // Plain binary.
        let f = facts("linux", "/home/u/bin/glance");
        assert_eq!(detect(&f), InstallKind::Binary("/home/u/bin/glance".into()));
        // Flatpak wins over everything.
        let mut fp = f.clone();
        fp.flatpak = true;
        fp.appimage = Some("/x.AppImage".into());
        assert_eq!(detect(&fp), InstallKind::Flatpak);
        // AppImage: $APPIMAGE plus running from inside its mount.
        let mut ai = facts("linux", "/tmp/.mount_glanceX/usr/bin/glance");
        ai.appimage = Some("/home/u/glance.AppImage".into());
        ai.appdir = Some("/tmp/.mount_glanceX".into());
        assert_eq!(
            detect(&ai),
            InstallKind::AppImage("/home/u/glance.AppImage".into())
        );
        // $APPIMAGE inherited from another AppImage that launched us.
        let mut inherited = f.clone();
        inherited.appimage = Some("/home/u/OtherApp.AppImage".into());
        inherited.appdir = Some("/tmp/.mount_OtherA".into());
        assert_eq!(
            detect(&inherited),
            InstallKind::Binary("/home/u/bin/glance".into())
        );
        // Dev: debug build, test identity, cargo target dir.
        let mut d = f.clone();
        d.debug_build = true;
        assert_eq!(detect(&d), InstallKind::Dev);
        d.test_override = true;
        assert_eq!(
            detect(&d),
            InstallKind::Binary("/home/u/bin/glance".into()),
            "GLANCE_UPDATE_URL lets dev builds update"
        );
        let mut id = f.clone();
        id.data_dir_override = true;
        assert_eq!(detect(&id), InstallKind::Dev);
        assert_eq!(
            detect(&facts("linux", "/src/glance/target/release/glance")),
            InstallKind::Dev
        );
        assert_eq!(
            detect(&facts(
                "linux",
                "/src/glance/target/x86_64-unknown-linux-gnu/release/glance"
            )),
            InstallKind::Dev
        );
        assert_eq!(
            detect(&facts("linux", "/opt/release/glance")),
            InstallKind::Binary("/opt/release/glance".into())
        );
        // macOS bundle vs bare binary.
        assert_eq!(
            detect(&facts(
                "macos",
                "/Applications/glance.app/Contents/MacOS/glance"
            )),
            InstallKind::MacApp("/Applications/glance.app".into())
        );
        assert_eq!(
            detect(&facts("macos", "/usr/local/bin/glance")),
            InstallKind::Binary("/usr/local/bin/glance".into())
        );
        // Windows installed / portable / loose exe.
        let win = |exe: &str, portable: bool| {
            let mut f = facts("windows", exe);
            f.local_app_data = Some("/Users/u/AppData/Local".into());
            f.portable_marker = portable;
            detect(&f)
        };
        let installed = "/Users/u/AppData/Local/Programs/glance/glance.exe";
        assert_eq!(
            win(installed, false),
            InstallKind::WindowsInstalled(installed.into())
        );
        assert_eq!(
            win(installed, true),
            InstallKind::WindowsPortable(installed.into())
        );
        assert_eq!(
            win("/Users/u/Desktop/glance/glance.exe", true),
            InstallKind::WindowsPortable("/Users/u/Desktop/glance/glance.exe".into())
        );
        assert_eq!(
            win("/Users/u/Downloads/glance.exe", false),
            InstallKind::Binary("/Users/u/Downloads/glance.exe".into())
        );
    }

    #[test]
    fn verifies_signatures() {
        let key = TestKey::new();
        let pk = parse_public_key(&key.public_file()).expect("key parses");
        let data = b"new glance build";
        let sig_text = key.sign(data, "glance-v0.2.0-linux-x86_64");
        let sig = parse_signature(&sig_text, "glance-v0.2.0-linux-x86_64").unwrap();
        verify(&pk, &sig, data).unwrap();
        assert!(verify(&pk, &sig, b"tampered build").is_err());
        // A signature for another file (an older version) is refused.
        assert!(parse_signature(&sig_text, "glance-v0.3.0-linux-x86_64").is_err());
        assert!(parse_signature(&sig_text, "glance-v0.2.0-linux-x86_64.Ap").is_err());
        // Another key's signature fails.
        let other = TestKey::new();
        let other_sig = parse_signature(
            &other.sign(data, "glance-v0.2.0-linux-x86_64"),
            "glance-v0.2.0-linux-x86_64",
        )
        .unwrap();
        assert!(verify(&pk, &other_sig, data).is_err());
        assert!(parse_signature("garbage", "x").is_err());
    }

    #[test]
    fn flathub_origin_is_read_from_flatpak_info() {
        assert!(from_flathub(
            "[Application]\nname=x\n\n[Instance]\norigin=flathub\n"
        ));
        assert!(!from_flathub("[Instance]\norigin=glance-origin\n"));
        assert!(!from_flathub(""));
    }

    #[test]
    fn placeholder_key_disables_updates() {
        let shipped = include_str!("../../../../packaging/minisign.pub");
        // Whatever is checked in must either parse or be the placeholder;
        // it must never panic.
        let _ = parse_public_key(shipped);
        assert!(parse_public_key("# placeholder\n# no key yet\n").is_none());
        assert!(parse_public_key("").is_none());
        assert!(parse_public_key("untrusted comment: x\nnot-base64!").is_none());
        let key = TestKey::new();
        let b64 = key.public_file().lines().nth(1).unwrap().to_string();
        assert!(parse_public_key(&b64).is_some(), "bare base64 works too");
    }
}
