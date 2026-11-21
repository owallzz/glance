//! Bridge between the gpui UI and glance-net: a small tokio runtime, the
//! persisted identity key and profile, and helpers that run network futures
//! off the UI thread.

use std::future::Future;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use glance_core::{PeerId, Profile};
use glance_net::{NetConfig, Node, Relay, RoomHandle};
use glance_proto::{MemberState, Ticket, WireProfile};
use iroh::SecretKey;
use serde::{Deserialize, Serialize};

/// Where glance keeps its identity, profile and settings.
/// `GLANCE_DATA_DIR` overrides it, which is how you run a second "person"
/// on one machine. A portable copy (see [`portable_dir`]) keeps its data in
/// `data` next to the executable; otherwise it's the OS data dir.
pub fn data_dir() -> PathBuf {
    choose_data_dir(
        std::env::var_os("GLANCE_DATA_DIR").map(PathBuf::from),
        portable_dir(),
        dirs::data_dir(),
    )
}

fn choose_data_dir(
    override_dir: Option<PathBuf>,
    portable: Option<PathBuf>,
    os_data: Option<PathBuf>,
) -> PathBuf {
    override_dir
        .or_else(|| portable.map(|d| d.join("data")))
        .or_else(|| os_data.map(|d| d.join("glance")))
        .unwrap_or_else(|| PathBuf::from(".glance"))
}

/// Name of the marker file that makes a copy portable.
pub const PORTABLE_MARKER: &str = "portable";

/// The folder of a portable copy: the executable's folder (the AppImage's
/// for an AppImage) when a file named `portable` sits in it. Portable copies
/// keep their data next to themselves and leave the system alone (no
/// start-on-login, no glance:// registration).
pub fn portable_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let appimage = std::env::var_os("APPIMAGE").map(PathBuf::from);
    portable_root(&exe, appimage.as_deref(), |p| p.is_file())
}

fn portable_root(
    exe: &Path,
    appimage: Option<&Path>,
    is_file: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    // An AppImage runs from a temporary mount; the marker sits next to the
    // .AppImage file itself.
    let dir = appimage.unwrap_or(exe).parent()?;
    is_file(&dir.join(PORTABLE_MARKER)).then(|| dir.to_path_buf())
}

/// Running inside a Flatpak sandbox: updates, start-on-login and the
/// glance:// handler are the sandbox's business.
pub fn in_flatpak() -> bool {
    std::env::var_os("FLATPAK_ID").is_some() || Path::new("/.flatpak-info").exists()
}

/// Whether this copy may change the system (start-on-login, link handler):
/// only release builds using the real data dir, never portable copies, so
/// dev builds and test identities don't take over the installed app.
pub fn may_integrate() -> bool {
    !cfg!(debug_assertions)
        && std::env::var_os("GLANCE_DATA_DIR").is_none()
        && portable_dir().is_none()
}

/// Load the identity key, creating it on first run (0600 on Unix).
pub fn load_or_create_key(dir: &Path) -> Result<SecretKey> {
    let path = dir.join("identity.key");
    if let Ok(bytes) = std::fs::read(&path) {
        let arr: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .context("identity.key must be 32 bytes")?;
        return Ok(SecretKey::from_bytes(&arr));
    }
    std::fs::create_dir_all(dir)?;
    let key = SecretKey::generate();
    write_private(&path, &key.to_bytes())?;
    Ok(key)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// The name and colour you chose, saved as profile.json.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SavedProfile {
    pub name: String,
    pub color: u32,
}

pub fn load_profile(dir: &Path) -> Option<SavedProfile> {
    let text = std::fs::read_to_string(dir.join("profile.json")).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn save_profile(dir: &Path, profile: &SavedProfile) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join("profile.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(profile)?)?;
    std::fs::rename(tmp, dir.join("profile.json"))?;
    Ok(())
}

/// Avatar colours offered to new users (Discord-like hues).
pub const AVATAR_COLORS: [u32; 8] = [
    0x5865f2, 0xe67e22, 0x1abc9c, 0xe91e63, 0x9b59b6, 0x3498db, 0xf1c40f, 0x2ecc71,
];

/// A stable default colour from the identity, so new users differ.
pub fn default_color(id: &PeerId) -> u32 {
    AVATAR_COLORS[id.0[0] as usize % AVATAR_COLORS.len()]
}

pub fn peer_id(id: glance_net::EndpointId) -> PeerId {
    PeerId(*id.as_bytes())
}

pub fn wire_profile(p: &Profile) -> WireProfile {
    WireProfile {
        name: p.name.clone(),
        color: p.color,
    }
}

/// Owns the runtime and the iroh node for the app's lifetime.
pub struct Net {
    rt: tokio::runtime::Runtime,
    node: Node,
    pub data_dir: PathBuf,
    /// Which relay this run uses, for settings.
    pub relay_label: String,
}

impl Net {
    pub fn start(data_dir: PathBuf) -> Result<Self> {
        let key = load_or_create_key(&data_dir)?;
        let files = data_dir.join("files");
        Self::start_with_key(data_dir, key, Some(files))
    }

    /// A throwaway identity (used by `glance bot`).
    pub fn start_ephemeral() -> Result<Self> {
        Self::start_with_key(
            std::env::temp_dir().join("glance-ephemeral"),
            SecretKey::generate(),
            None,
        )
    }

    fn start_with_key(
        data_dir: PathBuf,
        key: SecretKey,
        files_dir: Option<PathBuf>,
    ) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("glance-net")
            .enable_all()
            .build()?;
        // GLANCE_RELAY (testing) beats the saved setting.
        let saved = crate::app::Settings::load(&data_dir).relay_url;
        let relay = match std::env::var("GLANCE_RELAY").ok().or(saved).as_deref() {
            Some("off") => Relay::Disabled,
            Some(url) if !url.is_empty() => match url.parse() {
                Ok(url) => Relay::Custom(url),
                Err(e) => {
                    tracing::warn!("ignoring bad relay URL {url}: {e}");
                    Relay::Default
                }
            },
            _ => Relay::Default,
        };
        let relay_label = match &relay {
            Relay::Disabled => "None (direct connections only)".to_string(),
            Relay::Custom(url) => url.to_string(),
            Relay::Default => "Default public relays".to_string(),
        };
        let node = rt.block_on(Node::spawn(NetConfig {
            secret_key: key,
            relay,
            files_dir,
        }))?;
        Ok(Self {
            rt,
            node,
            data_dir,
            relay_label,
        })
    }

    pub fn me(&self) -> PeerId {
        peer_id(self.node.id())
    }

    /// Run `fut` on the network runtime; the returned future resolves on
    /// any executor (the UI awaits it).
    pub fn run<T: Send + 'static, F: Future<Output = T> + Send + 'static>(
        &self,
        fut: F,
    ) -> impl Future<Output = Option<T>> + use<T, F> {
        let (tx, rx) = flume::bounded(1);
        self.rt.spawn(async move {
            let _ = tx.send(fut.await);
        });
        async move { rx.recv_async().await.ok() }
    }

    pub fn create_room(
        &self,
        profile: WireProfile,
        state: MemberState,
    ) -> impl Future<Output = Result<RoomHandle>> + use<> {
        let node = self.node.clone();
        let fut = self.run(async move { node.create_room(profile, state).await });
        async move {
            fut.await
                .unwrap_or_else(|| Err(anyhow::anyhow!("network stopped")))
        }
    }

    pub fn join_room(
        &self,
        ticket: Ticket,
        profile: WireProfile,
        state: MemberState,
    ) -> impl Future<Output = Result<RoomHandle>> + use<> {
        let node = self.node.clone();
        let fut = self.run(async move { node.join_room(ticket, profile, state).await });
        async move {
            fut.await
                .unwrap_or_else(|| Err(anyhow::anyhow!("network stopped")))
        }
    }

    pub fn contacts(&self) -> glance_net::contacts::ContactsHandle {
        self.node.contacts().clone()
    }

    /// Add a file to our store (hash it) so it can be offered.
    pub fn add_file(&self, path: PathBuf) -> impl Future<Output = Result<([u8; 32], u64)>> + use<> {
        let node = self.node.clone();
        let fut = self.run(async move { node.files().add(&path).await });
        async move {
            fut.await
                .unwrap_or_else(|| Err(anyhow::anyhow!("network stopped")))
        }
    }

    /// Download a file from `providers`, then save it to `dest`. Progress
    /// goes to `progress`. Returns the task's abort handle (cancel).
    pub fn download_file(
        &self,
        hash: [u8; 32],
        providers: Vec<glance_net::EndpointId>,
        dest: PathBuf,
        progress: flume::Sender<glance_net::files::Progress>,
    ) -> tokio::task::AbortHandle {
        let node = self.node.clone();
        let handle = self.rt.spawn(async move {
            let p = progress.clone();
            let result = node
                .files()
                .download(node.endpoint(), hash, providers, move |ev| {
                    // Done is sent after the export below.
                    if !matches!(ev, glance_net::files::Progress::Done) {
                        let _ = p.try_send(ev);
                    }
                })
                .await;
            let result = match result {
                Ok(()) => node.files().export(hash, &dest).await,
                Err(e) => Err(e),
            };
            let _ = progress
                .send_async(match result {
                    Ok(()) => glance_net::files::Progress::Done,
                    Err(e) => glance_net::files::Progress::Failed(format!("{e:#}")),
                })
                .await;
        });
        handle.abort_handle()
    }

    /// Block a non-UI thread on a future (CLI modes only).
    pub fn block_on<F: Future>(&self, fut: F) -> F::Output {
        self.rt.block_on(fut)
    }

    /// Enter the runtime so a `RoomHandle` can be dropped from the UI thread
    /// (its Drop sends Leave through a channel; no runtime needed, but the
    /// guard keeps tokio-dependent drops safe).
    pub fn enter(&self) -> tokio::runtime::EnterGuard<'_> {
        self.rt.enter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_dir_prefers_override_then_portable_then_os() {
        let o = Some(PathBuf::from("/o"));
        let port = Some(PathBuf::from("/apps/glance"));
        let os = Some(PathBuf::from("/home/u/.local/share"));
        assert_eq!(
            choose_data_dir(o.clone(), port.clone(), os.clone()),
            PathBuf::from("/o")
        );
        assert_eq!(
            choose_data_dir(None, port, os.clone()),
            PathBuf::from("/apps/glance/data")
        );
        assert_eq!(
            choose_data_dir(None, None, os),
            PathBuf::from("/home/u/.local/share/glance")
        );
        assert_eq!(choose_data_dir(None, None, None), PathBuf::from(".glance"));
    }

    #[test]
    fn portable_needs_the_marker_next_to_the_exe() {
        let exe = Path::new("/apps/glance/glance.exe");
        let marker = |want: &'static str| move |p: &Path| p == Path::new(want);
        assert_eq!(
            portable_root(exe, None, marker("/apps/glance/portable")),
            Some(PathBuf::from("/apps/glance"))
        );
        assert_eq!(
            portable_root(exe, None, marker("/elsewhere/portable")),
            None
        );
        // AppImage: look next to the .AppImage, not inside its mount.
        let mounted = Path::new("/tmp/.mount_abc/usr/bin/glance");
        let image = Path::new("/home/u/Apps/glance.AppImage");
        assert_eq!(
            portable_root(mounted, Some(image), marker("/home/u/Apps/portable")),
            Some(PathBuf::from("/home/u/Apps"))
        );
        assert_eq!(
            portable_root(
                mounted,
                Some(image),
                marker("/tmp/.mount_abc/usr/bin/portable")
            ),
            None
        );
    }

    #[test]
    fn key_and_profile_persist() {
        let dir = std::env::temp_dir().join(format!("glance-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let k1 = load_or_create_key(&dir).unwrap();
        let k2 = load_or_create_key(&dir).unwrap();
        assert_eq!(k1.public(), k2.public());
        assert!(load_profile(&dir).is_none());
        let p = SavedProfile {
            name: "Sam".into(),
            color: 0x123456,
        };
        save_profile(&dir, &p).unwrap();
        assert_eq!(load_profile(&dir), Some(p));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
