//! End-to-end: a fake GitHub API on localhost serves a signed release; the
//! updater downloads, verifies, stages and replaces a dummy binary.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use semver::Version;

use super::release::test_keys::TestKey;
use super::release::{InstallKind, asset_name, parse_public_key};
use super::{Config, Status, apply, check_and_stage};

/// A tiny HTTP/1.1 server: path -> body, counting hits per path.
struct FakeServer {
    base: String,
    files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    hits: Arc<Mutex<HashMap<String, usize>>>,
}

impl FakeServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let files: Arc<Mutex<HashMap<String, Vec<u8>>>> = Default::default();
        let hits: Arc<Mutex<HashMap<String, usize>>> = Default::default();
        let (f, h) = (files.clone(), hits.clone());
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut req = Vec::new();
                let mut buf = [0u8; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&req);
                let path = text.split_whitespace().nth(1).unwrap_or("/").to_string();
                *h.lock().unwrap().entry(path.clone()).or_default() += 1;
                let body = f.lock().unwrap().get(&path).cloned();
                let head = match &body {
                    Some(b) => format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        b.len()
                    ),
                    None => {
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_string()
                    }
                };
                let _ = stream.write_all(head.as_bytes());
                if let Some(b) = body {
                    let _ = stream.write_all(&b);
                }
            }
        });
        Self { base, files, hits }
    }

    fn put(&self, path: &str, body: impl Into<Vec<u8>>) {
        self.files
            .lock()
            .unwrap()
            .insert(path.to_string(), body.into());
    }

    fn hits(&self, path: &str) -> usize {
        self.hits.lock().unwrap().get(path).copied().unwrap_or(0)
    }

    /// Publish `tag` with one asset (and its signature) for `kind`.
    fn release(&self, tag: &str, kind: &InstallKind, body: &[u8], sig: &str) -> String {
        let os = std::env::consts::OS;
        let arch = std::env::consts::ARCH;
        let name = asset_name(kind, tag, os, arch).unwrap();
        self.put(&format!("/dl/{name}"), body.to_vec());
        self.put(&format!("/dl/{name}.minisig"), sig.as_bytes().to_vec());
        let json = serde_json::json!({
            "tag_name": tag,
            "draft": false,
            "prerelease": false,
            "assets": [
                {"name": name, "size": body.len(), "browser_download_url": format!("{}/dl/{name}", self.base)},
                {"name": format!("{name}.minisig"), "size": sig.len(), "browser_download_url": format!("{}/dl/{name}.minisig", self.base)},
            ]
        });
        self.put("/repos/owallzz/glance/releases/latest", json.to_string());
        name
    }
}

static DIRS: AtomicUsize = AtomicUsize::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let n = DIRS.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("glance-update-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn config(server: &FakeServer, key: &TestKey, kind: InstallKind, dir: &Path) -> Config {
    Config {
        api_base: server.base.clone(),
        current: Version::new(0, 1, 0),
        kind,
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        key: parse_public_key(&key.public_file()),
        stage_dir: dir.join("updates"),
        unavailable: None,
    }
}

fn run(cfg: &Config) -> (anyhow::Result<Status>, Vec<Status>) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let seen = Mutex::new(Vec::new());
    let result = rt.block_on(check_and_stage(cfg, |s| seen.lock().unwrap().push(s)));
    (result, seen.into_inner().unwrap())
}

fn write_exe(path: &Path, body: &[u8]) {
    std::fs::write(path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[test]
fn downloads_verifies_stages_and_replaces_a_binary() {
    let dir = temp_dir("e2e");
    let target = dir.join("glance");
    write_exe(&target, b"old build");
    let key = TestKey::new();
    let server = FakeServer::start();
    let kind = InstallKind::Binary(target.clone());
    // Big enough to arrive in several chunks.
    let new_build: Vec<u8> = (0..600_000u32).map(|i| (i % 251) as u8).collect();
    let name = asset_name(
        &kind,
        "v9.0.0",
        std::env::consts::OS,
        std::env::consts::ARCH,
    )
    .unwrap();
    server.release("v9.0.0", &kind, &new_build, &key.sign(&new_build, &name));
    let cfg = config(&server, &key, kind.clone(), &dir);

    let (result, seen) = run(&cfg);
    let Status::Ready(staged) = result.unwrap() else {
        panic!("expected a staged update");
    };
    assert_eq!(staged.version, Version::new(9, 0, 0));
    assert_eq!(std::fs::read(&staged.path).unwrap(), new_build);
    assert_eq!(seen.first(), Some(&Status::Checking));
    assert!(seen.contains(&Status::Downloading(Version::new(9, 0, 0), 100)));
    assert_eq!(
        std::fs::read(&target).unwrap(),
        b"old build",
        "not applied yet"
    );

    // A second check reuses the staged file instead of downloading again.
    let (again, _) = run(&cfg);
    assert_eq!(again.unwrap(), Status::Ready(staged.clone()));
    assert_eq!(server.hits(&format!("/dl/{name}")), 1);

    // Restart: the dummy binary is replaced and stays executable.
    assert_eq!(
        apply::apply(&staged).unwrap(),
        apply::Next::Launch(target.clone())
    );
    assert_eq!(std::fs::read(&target).unwrap(), new_build);
    assert!(!staged.path.exists(), "staged file is cleaned up");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111);
    }

    // Now at 9.0.0: nothing to do.
    let mut newer = cfg.clone();
    newer.current = Version::new(9, 0, 0);
    assert_eq!(run(&newer).0.unwrap(), Status::UpToDate);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn refuses_a_tampered_download() {
    let dir = temp_dir("tamper");
    let target = dir.join("glance");
    write_exe(&target, b"old build");
    let key = TestKey::new();
    let server = FakeServer::start();
    let kind = InstallKind::Binary(target.clone());
    let name = asset_name(
        &kind,
        "v9.0.0",
        std::env::consts::OS,
        std::env::consts::ARCH,
    )
    .unwrap();
    let sig = key.sign(b"the real build", &name);
    server.release("v9.0.0", &kind, b"a malicious build", &sig);
    let cfg = config(&server, &key, kind.clone(), &dir);
    let err = run(&cfg).0.unwrap_err();
    assert!(format!("{err:#}").contains("signature"), "{err:#}");
    assert!(!cfg.stage_dir.join(&name).exists());
    assert!(
        std::fs::read_dir(&cfg.stage_dir).unwrap().next().is_none(),
        "partial download removed"
    );

    // A valid signature for an older release's file is refused too.
    let old_name = asset_name(
        &kind,
        "v0.0.9",
        std::env::consts::OS,
        std::env::consts::ARCH,
    )
    .unwrap();
    server.release(
        "v9.0.0",
        &kind,
        b"old signed build",
        &key.sign(b"old signed build", &old_name),
    );
    let err = run(&cfg).0.unwrap_err();
    assert!(format!("{err:#}").contains("different file"), "{err:#}");
    assert_eq!(std::fs::read(&target).unwrap(), b"old build");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn reports_without_downloading() {
    let dir = temp_dir("nodl");
    let key = TestKey::new();
    let server = FakeServer::start();
    // No release published yet: the API answers 404.
    let cfg = config(&server, &key, InstallKind::Binary(dir.join("glance")), &dir);
    assert_eq!(run(&cfg).0.unwrap(), Status::UpToDate);
    // Flatpak and dev builds never touch the network.
    let mut fp = cfg.clone();
    fp.unavailable = Some("Updates come from Flatpak".into());
    assert_eq!(
        run(&fp).0.unwrap(),
        Status::Unavailable("Updates come from Flatpak".into())
    );
    assert_eq!(server.hits("/repos/owallzz/glance/releases/latest"), 1);
    // A newer release whose build for this install isn't uploaded yet: a
    // transient state the timer retries soon.
    server.put(
        "/repos/owallzz/glance/releases/latest",
        r#"{"tag_name":"v9.0.0","assets":[]}"#,
    );
    let status = run(&cfg).0.unwrap();
    assert_eq!(status, Status::NotPublished(Version::new(9, 0, 0)));
    assert!(status.retry_soon());
    assert!(!Status::UpToDate.retry_soon());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn config_from_environment() {
    let key = TestKey::new();
    let pubkey = key.public_file();
    let dir = PathBuf::from("/data");
    let exe = PathBuf::from("/opt/glance/glance");
    let cfg_with = |vars: &[(&str, &str)], flatpak: bool| {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_vars(|k| map.get(k).cloned(), flatpak, exe.clone(), &dir)
    };
    // Test knobs are ignored without GLANCE_UPDATE_URL.
    let plain = cfg_with(
        &[
            ("GLANCE_UPDATE_CURRENT_VERSION", "0.0.1"),
            ("GLANCE_UPDATE_PUBKEY", &pubkey),
        ],
        false,
    );
    assert_eq!(plain.api_base, "https://api.github.com");
    assert_eq!(plain.current.to_string(), env!("CARGO_PKG_VERSION"));
    assert_eq!(plain.stage_dir, dir.join("updates"));
    if cfg!(debug_assertions) {
        assert_eq!(plain.kind, InstallKind::Dev);
        assert_eq!(plain.unavailable.as_deref(), Some("Development build"));
    }

    let testing = cfg_with(
        &[
            ("GLANCE_UPDATE_URL", "http://127.0.0.1:9/"),
            ("GLANCE_UPDATE_CURRENT_VERSION", "v0.0.1"),
            ("GLANCE_UPDATE_PUBKEY", &pubkey),
        ],
        false,
    );
    assert_eq!(testing.api_base, "http://127.0.0.1:9");
    assert_eq!(testing.current, Version::new(0, 0, 1));
    assert_eq!(testing.kind, InstallKind::Binary(exe.clone()));
    assert!(testing.key.is_some());
    assert_eq!(testing.unavailable, None);

    let off = cfg_with(
        &[("GLANCE_UPDATE_URL", "http://x"), ("GLANCE_UPDATE", "off")],
        false,
    );
    assert!(off.unavailable.unwrap().contains("turned off"));
    let flatpak = cfg_with(&[("GLANCE_UPDATE_URL", "http://x")], true);
    assert_eq!(flatpak.kind, InstallKind::Flatpak);
    assert_eq!(
        flatpak.unavailable.as_deref(),
        Some("Updates come from Flatpak")
    );
    // The checked-in placeholder key disables updates instead of panicking.
    let nokey = cfg_with(&[("GLANCE_UPDATE_URL", "http://x")], false);
    if parse_public_key(super::PUBLIC_KEY).is_none() {
        assert_eq!(
            nokey.unavailable.as_deref(),
            Some("Updates are disabled: no signing key")
        );
    }
}

#[test]
fn cleanup_removes_reached_versions() {
    let dir = temp_dir("cleanup");
    let stage = dir.join("updates");
    std::fs::create_dir_all(&stage).unwrap();
    for f in [
        "glance-v0.1.0-linux-x86_64",
        "glance-v0.2.0-windows-x86_64-setup.exe",
        "glance-v0.3.0-linux-x86_64.AppImage",
        "junk",
    ] {
        std::fs::write(stage.join(f), b"x").unwrap();
    }
    let exe = dir.join("glance.exe");
    std::fs::write(&exe, b"new").unwrap();
    std::fs::write(dir.join("glance.exe.old"), b"old").unwrap();
    std::fs::write(dir.join(".glance.exe.update"), b"half").unwrap();
    apply::cleanup(
        &InstallKind::WindowsPortable(exe.clone()),
        &stage,
        &Version::new(0, 2, 0),
    );
    let mut left: Vec<String> = std::fs::read_dir(&stage)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, ["glance-v0.3.0-linux-x86_64.AppImage"]);
    assert!(!dir.join("glance.exe.old").exists());
    assert!(!dir.join(".glance.exe.update").exists());
    assert!(exe.exists());

    // An interrupted macOS swap leaves work and backup dirs beside the app.
    let apps = dir.join("Applications");
    let bundle = apps.join("glance.app");
    for d in [
        &bundle,
        &apps.join(".glance-update/glance.app"),
        &apps.join(".glance.app.old"),
    ] {
        std::fs::create_dir_all(d).unwrap();
    }
    apply::cleanup(
        &InstallKind::MacApp(bundle.clone()),
        &stage,
        &Version::new(0, 2, 0),
    );
    let left: Vec<_> = std::fs::read_dir(&apps)
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(left, [std::ffi::OsString::from("glance.app")]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tray_item_only_while_ready() {
    let staged = super::Staged {
        version: Version::new(1, 2, 3),
        path: PathBuf::from("x"),
        kind: InstallKind::Dev,
    };
    assert_eq!(
        Status::Ready(staged).tray_label().as_deref(),
        Some("Restart to update to v1.2.3")
    );
    // Download progress doesn't change the tray item.
    assert_eq!(
        Status::Downloading(Version::new(1, 2, 3), 5).tray_label(),
        Status::Downloading(Version::new(1, 2, 3), 6).tray_label()
    );
    assert_eq!(Status::Installing(Version::new(1, 2, 3)).tray_label(), None);
    assert!(Status::Installing(Version::new(1, 2, 3)).busy());
}

#[cfg(unix)]
#[test]
fn replaces_an_app_bundle() {
    let dir = temp_dir("bundle");
    let make_app = |root: &Path, body: &str| {
        let macos = root.join("glance.app/Contents/MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        write_exe(&macos.join("glance"), body.as_bytes());
        std::fs::write(root.join("glance.app/Contents/Info.plist"), body).unwrap();
    };
    let apps = dir.join("Applications");
    make_app(&apps, "old");
    std::fs::write(apps.join("glance.app/Contents/stale-file"), b"x").unwrap();
    let build = dir.join("build");
    make_app(&build, "new");
    let archive = dir.join("glance-v9.0.0-macos-aarch64.app.tar.gz");
    let ok = std::process::Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(&build)
        .arg("glance.app")
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let bundle = apps.join("glance.app");
    let staged = super::Staged {
        version: Version::new(9, 0, 0),
        path: archive.clone(),
        kind: InstallKind::MacApp(bundle.clone()),
    };
    assert_eq!(
        apply::apply(&staged).unwrap(),
        apply::Next::Launch(bundle.join("Contents/MacOS/glance"))
    );
    assert_eq!(
        std::fs::read_to_string(bundle.join("Contents/MacOS/glance")).unwrap(),
        "new"
    );
    assert!(!bundle.join("Contents/stale-file").exists());
    // Only the bundle is left: no work or backup dirs.
    let left: Vec<_> = std::fs::read_dir(&apps).unwrap().flatten().collect();
    assert_eq!(left.len(), 1);
    // A broken archive leaves the old app alone.
    std::fs::write(&archive, b"not a tarball").unwrap();
    assert!(apply::replace_bundle(&archive, &bundle).is_err());
    assert!(bundle.join("Contents/MacOS/glance").is_file());
    let _ = std::fs::remove_dir_all(&dir);
}
