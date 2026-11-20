//! File sharing in a live room: send (attach button or drag & drop), offers
//! in chat, downloads with progress into the Downloads folder, inline image
//! previews, and re-serving files we have.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use glance_core::{ChatBody, ChatMessage, FileOffer, PeerId, Transfer};
use glance_net::files::Progress;
use glance_net::{Command, EndpointId};
use glance_proto::WireFile;
use gpui_kit::*;

use crate::app::{GlanceApp, now_ms};
use crate::net::peer_id;

/// Images up to this size download automatically for inline preview.
pub const AUTO_PREVIEW_MAX: u64 = 25 * 1000 * 1000;

/// What we know about one file offered in the room.
#[derive(Default)]
pub struct FileEntry {
    pub providers: Vec<EndpointId>,
    /// Where we saved it (or where it came from, for files we sent).
    pub path: Option<PathBuf>,
    pub abort: Option<tokio::task::AbortHandle>,
    pub _progress_task: Option<Task<()>>,
}

/// Files to send from a paste: copied files as they are, and pasted images
/// (screenshots) saved as files first. Empty for plain text.
pub fn pasted_files(item: &ClipboardItem) -> Vec<PathBuf> {
    pasted_files_into(item, &crate::net::data_dir().join("pasted"))
}

fn pasted_files_into(item: &ClipboardItem, dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in item.entries() {
        match entry {
            ClipboardEntry::ExternalPaths(p) => out.extend(p.paths().iter().cloned()),
            ClipboardEntry::Image(img) => {
                if let Some(path) = save_pasted_image(img, dir) {
                    out.push(path);
                }
            }
            ClipboardEntry::String(_) => {}
        }
    }
    out
}

/// Write a pasted image where the blob store can keep referencing it.
fn save_pasted_image(img: &Image, dir: &std::path::Path) -> Option<PathBuf> {
    let ext = match img.format {
        ImageFormat::Png => "png",
        ImageFormat::Jpeg => "jpg",
        ImageFormat::Webp => "webp",
        ImageFormat::Gif => "gif",
        ImageFormat::Svg => "svg",
        ImageFormat::Bmp => "bmp",
        ImageFormat::Tiff => "tiff",
        ImageFormat::Ico => "ico",
        ImageFormat::Pnm => "ppm",
    };
    std::fs::create_dir_all(dir).ok()?;
    let name = format!("Pasted image {}.{ext}", now_ms());
    let path = glance_net::files::unique_path(dir, &name);
    std::fs::write(&path, &img.bytes).ok()?;
    Some(path)
}

pub fn download_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("GLANCE_DOWNLOAD_DIR") {
        return PathBuf::from(dir);
    }
    dirs::download_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join("Downloads")))
        .unwrap_or_else(|| PathBuf::from("."))
}

impl GlanceApp {
    /// Ask the OS for files to send.
    pub fn pick_files_to_send(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Send".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                let _ = this.update(cx, |app, cx| app.send_files(paths, cx));
            }
        })
        .detach();
    }

    /// Hash each file and offer it to the room.
    pub fn send_files(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let Some(net) = self.net.clone() else { return };
        if self.room.as_ref().is_none_or(|r| r.handle.is_none()) {
            return;
        }
        for path in paths.into_iter().filter(|p| p.is_file()) {
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
                            app.post_file(app.me.id, file.clone(), Transfer::Sent, Some(path));
                            if let Some(h) = app.room.as_ref().and_then(|r| r.handle.as_ref()) {
                                h.send(Command::OfferFile(file));
                            }
                        }
                        Err(e) => app.system_message(format!("Couldn't send {name}: {e:#}")),
                    }
                    cx.notify();
                });
            })
            .detach();
        }
    }

    fn post_file(
        &mut self,
        author: PeerId,
        file: WireFile,
        transfer: Transfer,
        path: Option<PathBuf>,
    ) {
        let Some(room) = self.room.as_mut() else {
            return;
        };
        let entry = room.files.entry(file.hash).or_default();
        if path.is_some() {
            entry.path = path;
        }
        let id = room.chat.last().map_or(1, |m| m.id + 1);
        room.chat.push(ChatMessage {
            id,
            author,
            sent_at: now_ms(),
            body: ChatBody::File(FileOffer {
                hash: file.hash,
                name: file.name,
                size: file.size,
                mime: file.mime,
                transfer,
            }),
        });
    }

    pub fn on_file_offered(&mut self, from: EndpointId, file: WireFile, cx: &mut Context<Self>) {
        let auto = self.settings.auto_preview
            && file.mime.starts_with("image/")
            && file.size <= AUTO_PREVIEW_MAX;
        let hash = file.hash;
        if let Some(room) = self.room.as_mut() {
            let e = room.files.entry(hash).or_default();
            if !e.providers.contains(&from) {
                e.providers.push(from);
            }
        }
        self.post_file(peer_id(from), file, Transfer::Available, None);
        if auto {
            self.start_download(hash, cx);
        }
    }

    pub fn on_file_provider(&mut self, from: EndpointId, hash: [u8; 32]) {
        if let Some(e) = self.room.as_mut().and_then(|r| r.files.get_mut(&hash))
            && !e.providers.contains(&from)
        {
            e.providers.push(from);
        }
    }

    fn set_transfer(&mut self, hash: [u8; 32], transfer: Transfer) {
        if let Some(room) = self.room.as_mut() {
            for m in room.chat.iter_mut() {
                if let ChatBody::File(f) = &mut m.body
                    && f.hash == hash
                    && f.transfer != Transfer::Sent
                {
                    f.transfer = transfer.clone();
                }
            }
        }
    }

    /// Download button: start, or cancel if running.
    pub fn toggle_download(&mut self, hash: [u8; 32], cx: &mut Context<Self>) {
        let running = self
            .room
            .as_ref()
            .and_then(|r| r.files.get(&hash))
            .is_some_and(|e| e.abort.is_some());
        if running {
            if let Some(e) = self.room.as_mut().and_then(|r| r.files.get_mut(&hash)) {
                if let Some(a) = e.abort.take() {
                    a.abort();
                }
                e._progress_task = None;
            }
            self.set_transfer(hash, Transfer::Available);
        } else {
            self.start_download(hash, cx);
        }
        cx.notify();
    }

    fn start_download(&mut self, hash: [u8; 32], cx: &mut Context<Self>) {
        let Some(net) = self.net.clone() else { return };
        let Some(room) = self.room.as_mut() else {
            return;
        };
        let Some(offer) = room.chat.iter().find_map(|m| match &m.body {
            ChatBody::File(f) if f.hash == hash => Some(f.clone()),
            _ => None,
        }) else {
            return;
        };
        let entry = room.files.entry(hash).or_default();
        if entry.abort.is_some() {
            return;
        }
        let dest = glance_net::files::unique_path(&download_dir(), &offer.name);
        let (tx, rx) = flume::unbounded();
        entry.abort = Some(net.download_file(hash, entry.providers.clone(), dest.clone(), tx));
        let size = offer.size;
        let task = cx.spawn(async move |this, cx| {
            let start = Instant::now();
            while let Ok(p) = rx.recv_async().await {
                let done = matches!(p, Progress::Done | Progress::Failed(_));
                let _ = this.update(cx, |app, cx| {
                    match &p {
                        Progress::Bytes(n) => {
                            let secs = start.elapsed().as_secs_f64().max(0.2);
                            app.set_transfer(
                                hash,
                                Transfer::Downloading {
                                    done: (*n).min(size),
                                    bytes_per_sec: (*n as f64 / secs) as u64,
                                },
                            );
                        }
                        Progress::Done => {
                            app.set_transfer(hash, Transfer::Done);
                            if let Some(room) = app.room.as_mut() {
                                if let Some(e) = room.files.get_mut(&hash) {
                                    e.path = Some(dest.clone());
                                    e.abort = None;
                                }
                                if let Some(h) = &room.handle {
                                    h.send(Command::HaveFile(hash));
                                }
                            }
                        }
                        Progress::Failed(e) => {
                            app.set_transfer(hash, Transfer::Failed(e.clone()));
                            if let Some(entry) =
                                app.room.as_mut().and_then(|r| r.files.get_mut(&hash))
                            {
                                entry.abort = None;
                            }
                        }
                    }
                    cx.notify();
                });
                if done {
                    break;
                }
            }
        });
        if let Some(e) = self.room.as_mut().and_then(|r| r.files.get_mut(&hash)) {
            e._progress_task = Some(task);
        }
        self.set_transfer(
            hash,
            Transfer::Downloading {
                done: 0,
                bytes_per_sec: 0,
            },
        );
    }

    /// Where a finished file is on disk, if we have it.
    pub fn file_path(&self, hash: &[u8; 32]) -> Option<PathBuf> {
        self.room.as_ref()?.files.get(hash)?.path.clone()
    }

    pub fn reveal_file(&mut self, hash: [u8; 32], cx: &mut Context<Self>) {
        if let Some(path) = self.file_path(&hash) {
            cx.reveal_path(&path);
        }
    }
}

pub type FileMap = HashMap<[u8; 32], FileEntry>;

#[cfg(test)]
mod tests {
    use super::pasted_files_into;
    use gpui_kit::{ClipboardItem, Image, ImageFormat};

    #[test]
    fn pasting_text_sends_nothing_and_images_become_files() {
        let dir = std::env::temp_dir().join(format!("glance-paste-{}", std::process::id()));
        let text = ClipboardItem::new_string("hello".into());
        assert!(pasted_files_into(&text, &dir).is_empty());
        let png = crate::tray::icon_png(16);
        let img = ClipboardItem::new_image(&Image::from_bytes(ImageFormat::Png, png.clone()));
        let files = pasted_files_into(&img, &dir);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].extension().and_then(|e| e.to_str()), Some("png"));
        assert_eq!(std::fs::read(&files[0]).unwrap(), png);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
