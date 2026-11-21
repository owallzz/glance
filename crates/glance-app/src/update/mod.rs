//! Updates from GitHub Releases: check `releases/latest`, download the asset
//! for this install kind, verify its minisign signature against the key in
//! `packaging/minisign.pub`, stage it in the data dir, and apply it when the
//! user restarts. The UI side (timer, banner, settings) is `update_ui.rs`.
//!
//! Testing knobs, all only honoured while `GLANCE_UPDATE_URL` is set:
//! `GLANCE_UPDATE_URL` (base URL of a fake GitHub API; also lets dev builds
//! update), `GLANCE_UPDATE_CURRENT_VERSION` (pretend to be older) and
//! `GLANCE_UPDATE_PUBKEY` (a test public key). `GLANCE_UPDATE=off` turns
//! updates off.

pub mod apply;
pub mod cli;
pub mod release;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use minisign_verify::PublicKey;
use semver::Version;
use tokio::io::AsyncWriteExt;

pub use release::InstallKind;
use release::Plan;

pub const REPO: &str = "owallzz/glance";
const API_BASE: &str = "https://api.github.com";
/// The release signing key (a placeholder until the real key is added).
const PUBLIC_KEY: &str = include_str!("../../../../packaging/minisign.pub");
/// What a Flatpak bundle install is told about a newer release.
pub const FLATPAK_HINT: &str = "download the new .flatpak from the release page";
/// Where people download releases by hand.
pub const RELEASES_URL: &str = "https://github.com/owallzz/glance/releases/latest";
/// Nothing we ship is anywhere near this; refuse anything bigger.
const MAX_ASSET: u64 = 512 * 1024 * 1024;

/// How this process updates itself.
#[derive(Clone)]
pub struct Config {
    pub api_base: String,
    pub current: Version,
    pub kind: InstallKind,
    pub os: String,
    pub arch: String,
    pub key: Option<PublicKey>,
    /// Verified downloads wait here until applied.
    pub stage_dir: PathBuf,
    /// Why this install never downloads updates (Flatpak, dev build, ...).
    pub unavailable: Option<String>,
}

impl Config {
    pub fn from_env(data_dir: &Path) -> Self {
        let exe = std::env::current_exe().unwrap_or_default();
        Self::from_vars(
            |k| std::env::var(k).ok().filter(|v| !v.is_empty()),
            std::fs::read_to_string("/.flatpak-info").ok().as_deref(),
            exe,
            data_dir,
        )
    }

    /// `from_env` with the environment passed in (for tests).
    pub fn from_vars(
        var: impl Fn(&str) -> Option<String>,
        flatpak_info: Option<&str>,
        exe: PathBuf,
        data_dir: &Path,
    ) -> Self {
        let test_url = var("GLANCE_UPDATE_URL");
        let testing = test_url.is_some();
        let portable_marker = exe.parent().is_some_and(|d| d.join("portable").is_file());
        let facts = release::Facts {
            os: std::env::consts::OS.into(),
            debug_build: cfg!(debug_assertions),
            data_dir_override: var("GLANCE_DATA_DIR").is_some(),
            test_override: testing,
            flatpak: flatpak_info.is_some() || var("FLATPAK_ID").is_some(),
            appimage: var("APPIMAGE").map(PathBuf::from),
            appdir: var("APPDIR").map(PathBuf::from),
            exe,
            local_app_data: var("LOCALAPPDATA").map(PathBuf::from),
            portable_marker,
        };
        let kind = release::detect(&facts);
        let current = var("GLANCE_UPDATE_CURRENT_VERSION")
            .filter(|_| testing)
            .and_then(|v| Version::parse(v.trim_start_matches('v')).ok())
            .unwrap_or_else(|| Version::parse(env!("CARGO_PKG_VERSION")).expect("crate version"));
        let key = var("GLANCE_UPDATE_PUBKEY").filter(|_| testing).map_or_else(
            || release::parse_public_key(PUBLIC_KEY),
            |k| release::parse_public_key(&k),
        );
        let unavailable = if var("GLANCE_UPDATE").as_deref() == Some("off") {
            Some("Updates are turned off (GLANCE_UPDATE=off)".to_string())
        } else {
            match &kind {
                // Flathub updates it; a bundle installed from a release's
                // .flatpak file never updates, so that one checks and says
                // when there's a new .flatpak to download.
                InstallKind::Flatpak if flatpak_info.is_some_and(release::from_flathub) => {
                    Some("Updates come from Flathub".to_string())
                }
                InstallKind::Flatpak => None,
                InstallKind::Dev => Some("Development build".to_string()),
                _ if key.is_none() => Some("Updates are disabled: no signing key".to_string()),
                _ => None,
            }
        };
        Self {
            api_base: test_url
                .unwrap_or_else(|| API_BASE.to_string())
                .trim_end_matches('/')
                .to_string(),
            current,
            kind,
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            key,
            stage_dir: data_dir.join("updates"),
            unavailable,
        }
    }

    fn latest_url(&self) -> String {
        format!("{}/repos/{REPO}/releases/latest", self.api_base)
    }
}

/// An update that's downloaded, verified and waiting for a restart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Staged {
    pub version: Version,
    pub path: PathBuf,
    pub kind: InstallKind,
}

/// Where the updater is, for the settings page and the banner.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum Status {
    #[default]
    Idle,
    /// This install doesn't self-update (the reason is shown as is).
    Unavailable(String),
    Checking,
    UpToDate,
    /// Newer, but it can't be installed from here (why).
    Available(Version, String),
    /// Newer, but the build for this system (or its signature) isn't on the
    /// release yet. Checked again soon, not in 6 hours.
    NotPublished(Version),
    Downloading(Version, u8),
    Ready(Staged),
    /// Putting the staged update in place before the restart.
    Installing(Version),
    Error(String),
}

impl Status {
    pub fn text(&self) -> String {
        match self {
            Status::Idle => "Not checked yet".into(),
            Status::Unavailable(why) => why.clone(),
            Status::Checking => "Checking…".into(),
            Status::UpToDate => "Up to date".into(),
            Status::Available(v, why) => format!("v{v} is available: {why}"),
            Status::NotPublished(v) => {
                format!("v{v} is out, but its build for this system isn't published yet")
            }
            Status::Downloading(v, pct) => format!("Downloading v{v} · {pct}%"),
            Status::Ready(s) => format!("v{} is ready: restart to update", s.version),
            Status::Installing(v) => format!("Installing v{v}…"),
            Status::Error(e) => format!("Error: {e}"),
        }
    }

    pub fn busy(&self) -> bool {
        matches!(
            self,
            Status::Checking | Status::Downloading(..) | Status::Installing(_)
        )
    }

    /// Whether the next automatic check should come soon (with backoff)
    /// rather than in 6 hours: errors, and releases still being published.
    pub fn retry_soon(&self) -> bool {
        matches!(self, Status::Error(_) | Status::NotPublished(_))
    }

    /// The tray's "Restart to update" item for this status.
    pub fn tray_label(&self) -> Option<String> {
        match self {
            Status::Ready(s) => Some(format!("Restart to update to v{}", s.version)),
            _ => None,
        }
    }
}

/// A client with the same rustls (ring) stack iroh uses; no OpenSSL.
fn client() -> Result<reqwest::Client> {
    // Fails harmlessly when a provider is already installed.
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .user_agent("glance-updater")
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .build()
        .context("couldn't set up HTTPS")
}

/// Check for a newer release and, if there's one for this install, download,
/// verify and stage it. `progress` hears `Checking` and `Downloading`.
pub async fn check_and_stage(cfg: &Config, progress: impl Fn(Status)) -> Result<Status> {
    if let Some(why) = &cfg.unavailable {
        return Ok(Status::Unavailable(why.clone()));
    }
    let Some(key) = &cfg.key else {
        bail!("no signing key");
    };
    progress(Status::Checking);
    let http = client()?;
    let resp = http
        .get(cfg.latest_url())
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("couldn't reach GitHub")?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        // No releases yet.
        return Ok(Status::UpToDate);
    }
    let resp = resp.error_for_status().context("GitHub refused")?;
    let release = release::parse_release(&resp.text().await?)?;
    let (version, asset, signature) =
        match release::plan(&release, &cfg.current, &cfg.kind, &cfg.os, &cfg.arch) {
            Plan::UpToDate => return Ok(Status::UpToDate),
            Plan::NoAsset(v) if cfg.kind == InstallKind::Flatpak => {
                return Ok(Status::Available(v, FLATPAK_HINT.into()));
            }
            Plan::NoAsset(v) => {
                return Ok(Status::Available(
                    v,
                    "there's no build for this system".into(),
                ));
            }
            Plan::NotPublished(v) => return Ok(Status::NotPublished(v)),
            Plan::Download {
                version,
                asset,
                signature,
            } => (version, asset, signature),
        };
    if let Some(target) = cfg.kind.target()
        && let Err(e) = apply::check_writable(target)
    {
        return Ok(Status::Available(
            version,
            format!("can't replace {} ({e})", target.display()),
        ));
    }
    if asset.size > MAX_ASSET {
        bail!("the update is unexpectedly large");
    }

    let sig_text = get_small(&http, &signature.browser_download_url).await?;
    let sig = release::parse_signature(&sig_text, &asset.name)?;
    let staged_path = cfg.stage_dir.join(&asset.name);
    let staged = Staged {
        version: version.clone(),
        path: staged_path.clone(),
        kind: cfg.kind.clone(),
    };
    // Already downloaded on an earlier run? Check it again and reuse it.
    if verify_file(key, &sig, &staged_path).await.is_ok() {
        return Ok(Status::Ready(staged));
    }

    tokio::fs::create_dir_all(&cfg.stage_dir).await?;
    clear_stage(&cfg.stage_dir).await;
    let part = cfg.stage_dir.join(format!("{}.part", asset.name));
    let mut resp = http
        .get(&asset.browser_download_url)
        .send()
        .await
        .context("download failed")?
        .error_for_status()
        .context("download failed")?;
    let total = resp.content_length().unwrap_or(asset.size).max(1);
    if total > MAX_ASSET {
        bail!("the update is unexpectedly large");
    }
    let mut verifier = key
        .verify_stream(&sig)
        .map_err(|e| anyhow::anyhow!("unusable signature: {e}"))?;
    let mut file = tokio::fs::File::create(&part).await?;
    let (mut got, mut last_pct) = (0u64, u8::MAX);
    progress(Status::Downloading(version.clone(), 0));
    while let Some(chunk) = resp.chunk().await.context("download interrupted")? {
        got += chunk.len() as u64;
        if got > MAX_ASSET {
            bail!("the update is unexpectedly large");
        }
        verifier.update(&chunk);
        file.write_all(&chunk).await?;
        let pct = (got * 100 / total).min(100) as u8;
        if pct != last_pct {
            last_pct = pct;
            progress(Status::Downloading(version.clone(), pct));
        }
    }
    file.flush().await?;
    file.sync_all().await?;
    drop(file);
    if let Err(e) = verifier.finalize() {
        let _ = tokio::fs::remove_file(&part).await;
        bail!("the download's signature doesn't match ({e})");
    }
    tokio::fs::rename(&part, &staged_path).await?;
    Ok(Status::Ready(staged))
}

/// GET a small text file (a signature).
async fn get_small(http: &reqwest::Client, url: &str) -> Result<String> {
    let resp = http
        .get(url)
        .send()
        .await
        .context("couldn't download the signature")?
        .error_for_status()
        .context("couldn't download the signature")?;
    if resp.content_length().is_some_and(|n| n > 64 * 1024) {
        bail!("the signature file is too large");
    }
    let bytes = resp.bytes().await?;
    if bytes.len() > 64 * 1024 {
        bail!("the signature file is too large");
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Verify a file on disk against a signature, streaming it.
async fn verify_file(key: &PublicKey, sig: &minisign_verify::Signature, path: &Path) -> Result<()> {
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path).await?;
    let mut verifier = key
        .verify_stream(sig)
        .map_err(|e| anyhow::anyhow!("unusable signature: {e}"))?;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        verifier.update(&buf[..n]);
    }
    verifier
        .finalize()
        .map_err(|e| anyhow::anyhow!("bad signature: {e}"))
}

/// Remove older staged downloads.
async fn clear_stage(dir: &Path) {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(e)) = entries.next_entry().await {
        let _ = tokio::fs::remove_file(e.path()).await;
    }
}

#[cfg(test)]
mod tests;
