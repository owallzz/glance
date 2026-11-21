//! File sharing over iroh-blobs: files are content-addressed (BLAKE3),
//! verified while streaming, resumable, and any peer that has a file can
//! serve it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use iroh::Endpoint;
use iroh_blobs::api::Store;
use iroh_blobs::api::blobs::{AddPathOptions, ImportMode};
use iroh_blobs::api::downloader::DownloadProgressItem;
use iroh_blobs::api::proto::BlobStatus;
use iroh_blobs::{BlobFormat, Hash};
use n0_future::StreamExt;

use crate::EndpointId;

/// Owns the blob store (on disk in the data dir, or in memory).
#[derive(Clone)]
pub struct FileStore {
    store: Store,
    _owner: Arc<dyn std::any::Any + Send + Sync>,
}

impl std::fmt::Debug for FileStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FileStore")
    }
}

/// Download progress updates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Progress {
    /// Bytes received so far.
    Bytes(u64),
    Done,
    Failed(String),
}

impl FileStore {
    pub async fn open(dir: Option<&Path>) -> Result<Self> {
        Ok(match dir {
            Some(dir) => {
                std::fs::create_dir_all(dir)?;
                let fs = iroh_blobs::store::fs::FsStore::load(dir)
                    .await
                    .map_err(|e| anyhow::anyhow!("open file store: {e}"))?;
                Self {
                    store: (*fs).clone(),
                    _owner: Arc::new(fs),
                }
            }
            None => {
                let mem = iroh_blobs::store::mem::MemStore::new();
                Self {
                    store: (*mem).clone(),
                    _owner: Arc::new(mem),
                }
            }
        })
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Add a file (by reference when possible, so big files aren't copied)
    /// and keep it. Returns its hash and size.
    pub async fn add(&self, path: &Path) -> Result<([u8; 32], u64)> {
        let abs = std::path::absolute(path)?;
        let size = std::fs::metadata(&abs).context("read file")?.len();
        let tag = self
            .store
            .blobs()
            .add_path_with_opts(AddPathOptions {
                path: abs,
                format: BlobFormat::Raw,
                mode: ImportMode::TryReference,
            })
            .await
            .map_err(|e| anyhow::anyhow!("import file: {e}"))?;
        Ok((*tag.hash.as_bytes(), size))
    }

    pub async fn has(&self, hash: [u8; 32]) -> bool {
        self.store
            .blobs()
            .has(Hash::from_bytes(hash))
            .await
            .unwrap_or(false)
    }

    /// Fetch a blob from any of `providers`, reporting progress. Partial
    /// data is kept, so calling again resumes.
    pub async fn download(
        &self,
        endpoint: &Endpoint,
        hash: [u8; 32],
        providers: Vec<EndpointId>,
        progress: impl FnMut(Progress) + Send,
    ) -> Result<()> {
        self.download_at_most(endpoint, hash, providers, u64::MAX, progress)
            .await
    }

    /// [`download`](Self::download), but give up (and keep nothing) once
    /// more than `max_size` bytes arrive: a peer's offer states the size,
    /// and a blob bigger than that isn't the file we agreed to fetch.
    pub async fn download_at_most(
        &self,
        endpoint: &Endpoint,
        hash: [u8; 32],
        providers: Vec<EndpointId>,
        max_size: u64,
        mut progress: impl FnMut(Progress) + Send,
    ) -> Result<()> {
        let hash = Hash::from_bytes(hash);
        let downloader = self.store.downloader(endpoint);
        let mut stream = downloader
            .download(hash, providers)
            .stream()
            .await
            .map_err(|e| anyhow::anyhow!("start download: {e}"))?;
        let mut failed = None;
        while let Some(item) = stream.next().await {
            match item {
                DownloadProgressItem::Progress(n) if n > max_size => {
                    // Dropping the stream stops the download.
                    drop(stream);
                    let msg = "the file is bigger than its sender said".to_string();
                    progress(Progress::Failed(msg.clone()));
                    anyhow::bail!(msg)
                }
                DownloadProgressItem::Progress(n) => progress(Progress::Bytes(n)),
                DownloadProgressItem::Error(e) => failed = Some(e.to_string()),
                DownloadProgressItem::DownloadError => {
                    failed = Some("no one who has the file could send it".into())
                }
                _ => {}
            }
        }
        let status = self.store.blobs().status(hash).await;
        if let Ok(BlobStatus::Complete { size }) = status
            && size > max_size
        {
            let msg = "the file is bigger than its sender said".to_string();
            progress(Progress::Failed(msg.clone()));
            anyhow::bail!(msg)
        }
        if matches!(status, Ok(BlobStatus::Complete { .. })) {
            // Keep it from garbage collection and so we can re-serve it.
            let _ = self.store.tags().create(hash).await;
            progress(Progress::Done);
            Ok(())
        } else {
            let msg = failed.unwrap_or_else(|| "download incomplete".into());
            progress(Progress::Failed(msg.clone()));
            anyhow::bail!(msg)
        }
    }

    /// Copy a complete blob out to `dest`.
    pub async fn export(&self, hash: [u8; 32], dest: &Path) -> Result<()> {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        self.store
            .blobs()
            .export(Hash::from_bytes(hash), std::path::absolute(dest)?)
            .await
            .map_err(|e| anyhow::anyhow!("save file: {e}"))?;
        Ok(())
    }
}

/// A free path in `dir` for `name` ("report.pdf", then "report (1).pdf"...).
/// Strips directory components so a peer can't write outside `dir`.
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let clean: String = Path::new(name)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let clean = if clean.trim().is_empty() || clean == "." || clean == ".." {
        "download".to_string()
    } else {
        clean
    };
    let candidate = dir.join(&clean);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match clean.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (clean.clone(), String::new()),
    };
    (1..)
        .map(|i| dir.join(format!("{stem} ({i}){ext}")))
        .find(|p| !p.exists())
        .expect("some name is free")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_paths_are_safe_and_free() {
        let dir = std::env::temp_dir().join(format!("glance-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a.txt"));
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a (1).txt"));
        assert_eq!(unique_path(&dir, "../../etc/passwd"), dir.join("passwd"));
        assert_eq!(unique_path(&dir, ".."), dir.join("download"));
        assert_eq!(
            unique_path(&dir, "C:\\x\\evil.exe").parent(),
            Some(dir.as_path())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
