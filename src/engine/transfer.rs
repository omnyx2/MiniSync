//! File I/O helpers: apply received file, compute SHA-256, generate conflict paths.

use anyhow::Result;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

use crate::index::FileEntry;

/// Write a received file to disk, creating parent directories as needed.
pub fn apply_file(root: &Path, entry: &FileEntry, contents: &[u8]) -> Result<()> {
    let dest = root.join(&entry.path);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&dest, contents)?;
    Ok(())
}

/// Compute the SHA-256 hex digest of data.
pub fn sha256_hex(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

/// Generate a conflict filename: `report.pdf` + peer `a1b2` → `report.conflict-a1b2.pdf`
pub fn conflict_path(rel: &str, peer_id: &str) -> String {
    let p = std::path::Path::new(rel);
    let stem = p.file_stem().unwrap_or_default().to_string_lossy();
    let ext = p
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let parent = p
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| format!("{}/", p.to_string_lossy().replace('\\', "/")))
        .unwrap_or_default();
    format!("{parent}{stem}.conflict-{peer_id}{ext}")
}
