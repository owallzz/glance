//! Files sent straight to a contact, outside any call. The file stays on
//! the sender's disk (in their blob store); the contact gets a card on the
//! home screen and a notification, and downloads it from the sender.

use std::path::PathBuf;
use std::time::Instant;

use glance_core::{FileOffer, PeerId, Transfer, format};
use glance_net::contacts::ContactCommand;
use glance_net::files::Progress;
use glance_proto::WireFile;
use gpui_kit::assets::IconName;
use gpui_kit::*;

use crate::app::GlanceApp;
use crate::net::peer_id;

fn endpoint_id(p: PeerId) -> Option<glance_net::EndpointId> {
    glance_net::EndpointId::from_bytes(&p.0).ok()
}
use crate::theme::Palette;
use crate::ui::{icon, icon_button};

/// A file sent to, or received from, a contact.
pub struct DirectFile {
    /// Who sent it (incoming) or who it went to (outgoing).
    pub peer: PeerId,
    pub incoming: bool,
    pub offer: FileOffer,
    /// Where it is on disk (the original for sent files).
    pub path: Option<PathBuf>,
    pub abort: Option<tokio::task::AbortHandle>,
    pub _task: Option<Task<()>>,
}

/// Keep the home list short.
const KEEP: usize = 30;

impl GlanceApp {
    /// "Send file" on a contact: pick files and send each one.
    pub fn send_file_to_contact(&mut self, to: PeerId, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Send".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                let _ = this.update(cx, |app, cx| {
                    for path in paths.into_iter().filter(|p| p.is_file()) {
                        app.send_path_to_contact(to, path, cx);
                    }
                });
            }
        })
        .detach();
    }

    fn send_path_to_contact(&mut self, to: PeerId, path: PathBuf, cx: &mut Context<Self>) {
        let Some(net) = self.net.clone() else { return };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into());
        let mime = mime_guess::from_path(&path)
            .first_or_octet_stream()
            .to_string();
        let fut = net.add_file(path.clone());
        cx.spawn(async move |this, cx| {
            let result = fut.await;
            let _ = this.update(cx, |app, cx| {
                match result {
                    Ok((hash, size)) => {
                        let file = WireFile {
                            hash,
                            name,
                            size,
                            mime,
                        };
                        if let (Some(net), Some(to)) = (&app.net, endpoint_id(to)) {
                            net.contacts().send(ContactCommand::SendFile {
                                to,
                                file: file.clone(),
                            });
                        }
                        app.push_direct(DirectFile {
                            peer: to,
                            incoming: false,
                            offer: offer(file, Transfer::Sent),
                            path: Some(path),
                            abort: None,
                            _task: None,
                        });
                    }
                    Err(e) => app.join_error = Some(format!("Couldn't send {name}: {e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// A contact sent us a file.
    pub fn on_contact_file(&mut self, from: glance_net::EndpointId, file: WireFile) {
        let who = self
            .profile_of(peer_id(from))
            .map(|p| p.name)
            .unwrap_or_else(|| "A contact".into());
        crate::contacts::notify(
            &format!("{who} sent you a file"),
            &format!("{} ({})", file.name, format::human_bytes(file.size)),
        );
        // A resend of the same file replaces the old card.
        self.direct_files
            .retain(|d| !(d.incoming && d.offer.hash == file.hash));
        self.push_direct(DirectFile {
            peer: peer_id(from),
            incoming: true,
            offer: offer(file, Transfer::Available),
            path: None,
            abort: None,
            _task: None,
        });
    }

    fn push_direct(&mut self, file: DirectFile) {
        self.direct_files.insert(0, file);
        self.direct_files.truncate(KEEP);
    }

    fn direct_mut(&mut self, hash: [u8; 32]) -> Option<&mut DirectFile> {
        self.direct_files
            .iter_mut()
            .find(|d| d.incoming && d.offer.hash == hash)
    }

    /// Download (or cancel) a file a contact sent.
    pub fn toggle_direct_download(&mut self, hash: [u8; 32], cx: &mut Context<Self>) {
        let Some(net) = self.net.clone() else { return };
        let Some(d) = self.direct_mut(hash) else {
            return;
        };
        if let Some(a) = d.abort.take() {
            a.abort();
            d._task = None;
            d.offer.transfer = Transfer::Available;
            cx.notify();
            return;
        }
        let Some(from) = endpoint_id(d.peer) else {
            return;
        };
        let dest = glance_net::files::unique_path(&crate::files::download_dir(), &d.offer.name);
        let (tx, rx) = flume::unbounded();
        d.abort = Some(net.download_file(hash, vec![from], dest.clone(), tx));
        d.offer.transfer = Transfer::Downloading {
            done: 0,
            bytes_per_sec: 0,
        };
        let size = d.offer.size;
        let task = cx.spawn(async move |this, cx| {
            let start = Instant::now();
            while let Ok(p) = rx.recv_async().await {
                let finished = matches!(p, Progress::Done | Progress::Failed(_));
                let _ = this.update(cx, |app, cx| {
                    if let Some(d) = app.direct_mut(hash) {
                        match p {
                            Progress::Bytes(n) => {
                                let secs = start.elapsed().as_secs_f64().max(0.2);
                                d.offer.transfer = Transfer::Downloading {
                                    done: n.min(size),
                                    bytes_per_sec: (n as f64 / secs) as u64,
                                };
                            }
                            Progress::Done => {
                                d.offer.transfer = Transfer::Done;
                                d.path = Some(dest.clone());
                                d.abort = None;
                            }
                            Progress::Failed(e) => {
                                d.offer.transfer = Transfer::Failed(e);
                                d.abort = None;
                            }
                        }
                    }
                    cx.notify();
                });
                if finished {
                    break;
                }
            }
        });
        if let Some(d) = self.direct_mut(hash) {
            d._task = Some(task);
        }
        cx.notify();
    }

    /// The home screen's list of files sent to / from contacts.
    pub fn direct_file_card(&self, p: &Palette, i: usize, cx: &mut Context<Self>) -> AnyElement {
        let d = &self.direct_files[i];
        let f = &d.offer;
        let who = self
            .profile_of(d.peer)
            .map(|p| p.name)
            .unwrap_or_else(|| "someone".into());
        let (status, progress) = match &f.transfer {
            Transfer::Available => (
                format!("From {who} · {}", format::human_bytes(f.size)),
                None,
            ),
            Transfer::Sent => (
                format!("Sent to {who} · {}", format::human_bytes(f.size)),
                None,
            ),
            Transfer::Done => (format!("From {who} · saved to Downloads"), None),
            Transfer::Failed(e) => (format!("Failed: {e}"), None),
            Transfer::Downloading { done, .. } => (
                format!(
                    "{} of {}",
                    format::human_bytes(*done),
                    format::human_bytes(f.size)
                ),
                Some(*done as f32 / f.size.max(1) as f32),
            ),
        };
        let hash = f.hash;
        let incoming = d.incoming;
        let path = d.path.clone();
        let action = match &f.transfer {
            Transfer::Available | Transfer::Failed(_) => Some((IconName::Download, "Download")),
            Transfer::Downloading { .. } => Some((IconName::X, "Cancel")),
            Transfer::Done | Transfer::Sent => path
                .as_ref()
                .map(|_| (IconName::FolderOpen, "Show in folder")),
        };
        div()
            .p_3()
            .rounded(px(8.))
            .bg(p.bg_sidebar)
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(icon(IconName::FileText, 24., p.brand))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_color(p.text_normal)
                                    .truncate()
                                    .child(f.name.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(p.text_muted)
                                    .truncate()
                                    .child(status),
                            ),
                    )
                    .children(action.map(|(glyph, tip)| {
                        icon_button(
                            SharedString::from(format!("direct-{i}-{tip}")),
                            glyph,
                            tip,
                            p.interactive,
                            p,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            match &path {
                                Some(path) if tip == "Show in folder" => cx.reveal_path(path),
                                _ if incoming => this.toggle_direct_download(hash, cx),
                                _ => {}
                            }
                        }))
                    })),
            )
            .children(progress.map(|frac| {
                div()
                    .h(px(4.))
                    .w_full()
                    .rounded_full()
                    .bg(p.bg_selected)
                    .child(div().h_full().w(relative(frac)).rounded_full().bg(p.brand))
            }))
            .into_any_element()
    }
}

fn offer(file: WireFile, transfer: Transfer) -> FileOffer {
    FileOffer {
        hash: file.hash,
        name: file.name,
        size: file.size,
        mime: file.mime,
        transfer,
    }
}
