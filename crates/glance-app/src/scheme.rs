//! Registering glance as the handler for `glance://` links, so clicking an
//! invite opens (or hands off to) the app.
//!
//! - Linux: a desktop entry in `~/.local/share/applications` claiming
//!   `x-scheme-handler/glance`, set as the default with `xdg-mime`.
//! - Windows: `HKCU\Software\Classes\glance`.
//! - macOS: declared in the app bundle's Info.plist (see `packaging/`);
//!   links arrive through `App::on_open_urls`.
//!
//! Like start-on-login, only release builds that use the real data dir do
//! this, so dev builds and test identities never take over the handler.

pub fn register() {
    if cfg!(debug_assertions) || std::env::var_os("GLANCE_DATA_DIR").is_some() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    if let Err(e) = register_for(&exe) {
        tracing::warn!("couldn't register glance:// links: {e:#}");
    }
}

#[cfg(target_os = "linux")]
fn register_for(exe: &std::path::Path) -> anyhow::Result<()> {
    let Some(apps) = dirs::data_dir().map(|d| d.join("applications")) else {
        return Ok(());
    };
    // AppImages run from a temporary mount; point at the AppImage itself.
    let exe = std::env::var_os("APPIMAGE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| exe.to_path_buf());
    let entry = desktop_entry(&exe.to_string_lossy());
    let path = apps.join("glance.desktop");
    if std::fs::read_to_string(&path).ok().as_deref() == Some(entry.as_str()) {
        return Ok(());
    }
    std::fs::create_dir_all(&apps)?;
    std::fs::write(&path, entry)?;
    let _ = std::process::Command::new("xdg-mime")
        .args(["default", "glance.desktop", "x-scheme-handler/glance"])
        .status();
    let _ = std::process::Command::new("update-desktop-database")
        .arg(&apps)
        .status();
    Ok(())
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn desktop_entry(exe: &str) -> String {
    // Quote the path per the desktop entry spec.
    let quoted = format!(
        "\"{}\"",
        exe.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('`', "\\`")
            .replace('$', "\\$")
    );
    format!(
        "[Desktop Entry]\nType=Application\nName=glance\nComment=Peer-to-peer screen sharing\nExec={quoted} %u\nIcon=glance\nTerminal=false\nCategories=Network;AudioVideo;\nMimeType=x-scheme-handler/glance;\nStartupWMClass=glance\n"
    )
}

#[cfg(target_os = "windows")]
fn register_for(exe: &std::path::Path) -> anyhow::Result<()> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey(r"Software\Classes\glance")?;
    key.set_value("", &"URL:glance")?;
    key.set_value("URL Protocol", &"")?;
    let (cmd, _) = key.create_subkey(r"shell\open\command")?;
    cmd.set_value("", &format!("\"{}\" \"%1\"", exe.display()))?;
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn register_for(_exe: &std::path::Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn desktop_entry_quotes_the_path() {
        let e = super::desktop_entry("/opt/my apps/glance");
        assert!(e.contains("Exec=\"/opt/my apps/glance\" %u\n"));
        assert!(e.contains("MimeType=x-scheme-handler/glance;"));
    }
}
