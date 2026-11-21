//! Installing a staged update for each install kind, and relaunching.
//!
//! - Plain binary, AppImage, Windows portable: copy the new file next to the
//!   old one and rename it over (atomic on Unix; on Windows the running exe
//!   is renamed to `glance.exe.old` first and removed on the next start).
//! - macOS app: unpack the `.app.tar.gz` beside the bundle with the system
//!   `tar` (keeps symlinks, modes and the code signature), then swap the
//!   bundles by renaming. Files downloaded by our own HTTP client get no
//!   quarantine attribute, so Gatekeeper doesn't stop the relaunch.
//! - Windows installed: run the new setup.exe silently. With `/LAUNCH=1` its
//!   `[Run]` entry (`Check: LaunchAfterSilentInstall`) starts glance again
//!   once it's done; without it the installer only updates.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use super::{InstallKind, Staged};

/// The flag a relaunched glance gets: wait for the old process to exit
/// before taking over the single-instance hand-off.
pub const AFTER_UPDATE: &str = "--after-update";

/// Tells the installer to start glance again after a silent upgrade
/// (`LaunchAfterSilentInstall` in `packaging/windows/glance.iss`).
pub const SETUP_LAUNCH: &str = "/LAUNCH=1";

/// Arguments for the silent Inno Setup upgrade that relaunches glance.
pub const SETUP_ARGS: [&str; 5] = [
    "/VERYSILENT",
    "/SUPPRESSMSGBOXES",
    "/NORESTART",
    "/CLOSEAPPLICATIONS",
    SETUP_LAUNCH,
];

/// The installer's arguments, with or without the relaunch.
pub fn setup_args(relaunch: bool) -> Vec<&'static str> {
    SETUP_ARGS
        .into_iter()
        .filter(|a| relaunch || *a != SETUP_LAUNCH)
        .collect()
}

/// Can we create files next to `target` (needed to replace it)?
pub fn check_writable(target: &Path) -> io::Result<()> {
    let dir = target
        .parent()
        .ok_or_else(|| io::Error::other("no parent directory"))?;
    let probe = dir.join(format!(".glance-write-test-{}", std::process::id()));
    std::fs::File::create(&probe).map_err(|e| match e.kind() {
        io::ErrorKind::PermissionDenied => io::Error::other("no write access"),
        _ => e,
    })?;
    std::fs::remove_file(&probe)
}

/// What to start once the update is in place.
#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    Launch(PathBuf),
    /// The installer finishes the update on its own (and starts glance
    /// again if `apply` was asked to relaunch).
    InstallerRelaunches,
}

/// Put the staged update in place. Nothing is relaunched yet, except by
/// the Windows installer, which starts glance afterwards when `relaunch`.
pub fn apply(staged: &Staged, relaunch: bool) -> Result<Next> {
    let next = match &staged.kind {
        InstallKind::Binary(p) | InstallKind::WindowsPortable(p) | InstallKind::AppImage(p) => {
            replace_file(&staged.path, p)?;
            Next::Launch(p.clone())
        }
        InstallKind::MacApp(bundle) => {
            replace_bundle(&staged.path, bundle)?;
            Next::Launch(bundle.join("Contents").join("MacOS").join("glance"))
        }
        InstallKind::WindowsInstalled(_) => {
            spawn_detached(&staged.path, &setup_args(relaunch))
                .context("couldn't start the installer")?;
            // setup.exe runs from the staging dir; it's cleaned on next start.
            return Ok(Next::InstallerRelaunches);
        }
        InstallKind::Flatpak | InstallKind::Dev => bail!("this install doesn't self-update"),
    };
    let _ = std::fs::remove_file(&staged.path);
    Ok(next)
}

/// Atomically replace `target` with a copy of `new`, keeping it executable.
pub fn replace_file(new: &Path, target: &Path) -> Result<()> {
    let dir = target.parent().context("no parent directory")?;
    let name = target
        .file_name()
        .context("no file name")?
        .to_string_lossy()
        .into_owned();
    let tmp = dir.join(format!(".{name}.update"));
    std::fs::copy(new, &tmp).with_context(|| format!("couldn't write {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(target)
            .map(|m| m.permissions().mode())
            .unwrap_or(0o755)
            | 0o755;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode & 0o7777))?;
    }
    #[cfg(windows)]
    {
        // A running exe can't be overwritten, but it can be renamed.
        let old = dir.join(format!("{name}.old"));
        let _ = std::fs::remove_file(&old);
        if target.exists() {
            std::fs::rename(target, &old).context("couldn't move the old version aside")?;
        }
        if let Err(e) = std::fs::rename(&tmp, target) {
            let _ = std::fs::rename(&old, target);
            let _ = std::fs::remove_file(&tmp);
            return Err(e).context("couldn't put the new version in place");
        }
    }
    #[cfg(not(windows))]
    if let Err(e) = std::fs::rename(&tmp, target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).context("couldn't put the new version in place");
    }
    Ok(())
}

/// Replace the app bundle at `bundle` with the one in a `.app.tar.gz`.
#[cfg(unix)]
pub fn replace_bundle(archive: &Path, bundle: &Path) -> Result<()> {
    let parent = bundle.parent().context("no parent directory")?;
    let work = parent.join(".glance-update");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;
    let result = (|| {
        let status = Command::new("tar")
            .arg("-xzf")
            .arg(archive)
            .arg("-C")
            .arg(&work)
            .stdin(Stdio::null())
            .status()
            .context("couldn't run tar")?;
        if !status.success() {
            bail!("couldn't unpack the update");
        }
        let new_app = std::fs::read_dir(&work)?
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|e| e == "app"))
            .context("the update has no app bundle")?;
        if !new_app.join("Contents/MacOS/glance").is_file() {
            bail!("the update's app bundle is incomplete");
        }
        let old = parent.join(".glance.app.old");
        let _ = std::fs::remove_dir_all(&old);
        std::fs::rename(bundle, &old).context("couldn't move the old app aside")?;
        if let Err(e) = std::fs::rename(&new_app, bundle) {
            let _ = std::fs::rename(&old, bundle);
            return Err(e).context("couldn't put the new app in place");
        }
        let _ = std::fs::remove_dir_all(&old);
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&work);
    result
}

#[cfg(not(unix))]
pub fn replace_bundle(_archive: &Path, _bundle: &Path) -> Result<()> {
    bail!("app bundles only exist on macOS")
}

/// Start the new version, detached from us (we're about to quit).
pub fn relaunch(exe: &Path, args: &[&str]) -> Result<()> {
    spawn_detached(exe, args).with_context(|| format!("couldn't start {}", exe.display()))
}

fn spawn_detached(exe: &Path, args: &[&str]) -> io::Result<()> {
    let mut cmd = Command::new(exe);
    cmd.args(args).stdin(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn().map(drop)
}

/// After an update (or one that was interrupted): remove what `apply` leaves
/// beside the install (the old Windows exe, a half-copied `.<name>.update`,
/// the macOS work and backup dirs) and staged downloads for versions we've
/// already reached.
pub fn cleanup(kind: &InstallKind, stage_dir: &Path, current: &semver::Version) {
    if let Some(target) = kind.target()
        && target.exists()
        && let (Some(dir), Some(name)) = (target.parent(), target.file_name())
    {
        let name = name.to_string_lossy();
        if matches!(kind, InstallKind::MacApp(_)) {
            let _ = std::fs::remove_dir_all(dir.join(".glance-update"));
            let _ = std::fs::remove_dir_all(dir.join(".glance.app.old"));
        } else {
            let _ = std::fs::remove_file(dir.join(format!("{name}.old")));
            let _ = std::fs::remove_file(dir.join(format!(".{name}.update")));
        }
    }
    let Ok(entries) = std::fs::read_dir(stage_dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let stale = name
            .strip_prefix("glance-v")
            .and_then(|rest| rest.split('-').next())
            .and_then(|v| semver::Version::parse(v).ok())
            .is_none_or(|v| v <= *current);
        if stale {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

#[cfg(test)]
mod setup_tests {
    use super::{SETUP_ARGS, SETUP_LAUNCH, setup_args};

    #[test]
    fn silent_upgrade_asks_the_installer_to_relaunch() {
        assert!(SETUP_ARGS.contains(&"/LAUNCH=1"));
        assert!(setup_args(true).contains(&SETUP_LAUNCH));
        assert!(!setup_args(false).contains(&SETUP_LAUNCH));
        assert!(setup_args(false).contains(&"/VERYSILENT"));
        // The installer only relaunches when it reads that parameter.
        let iss = include_str!("../../../../packaging/windows/glance.iss");
        assert!(iss.contains("param:LAUNCH"));
    }
}
