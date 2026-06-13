//! Building a snapshot ("index") of the sync folder: one entry per file.
//!
//! This is the foundation of any sync engine: before you can decide *what* to
//! send, you need a comparable description of *what each side has*.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use unicode_normalization::UnicodeNormalization;
use walkdir::WalkDir;

use crate::routing;

/// Canonicalize a relative path string for the wire/catalog: forward slashes +
/// Unicode NFC. macOS stores filenames as NFD (decomposed); Windows/Linux expect
/// NFC. Without this, a Korean filename round-trips as garbled jamo and — worse —
/// is treated as a *different* path on NFC filesystems, breaking dedup/merge.
/// Every node normalizes the same way, so paths are byte-identical fleet-wide.
pub fn normalize_rel(s: &str) -> String {
    s.replace('\\', "/").nfc().collect()
}

/// One file's metadata, relative to the sync root.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// Path relative to the sync root, always forward-slashed for portability.
    pub path: String,
    pub size: u64,
    /// Unix seconds. Fallback when version vectors are both empty.
    pub mtime: i64,
    /// SHA-256 hex digest — the identity check that tells two files apart.
    pub hash: String,
    /// Version vector: peer_id → edit count. Used by the file lane to detect
    /// concurrent modifications vs. strictly-newer updates.
    pub version: HashMap<String, u64>,
}

/// Map of relative-path -> entry. The whole-folder snapshot.
pub type Index = HashMap<String, FileEntry>;

#[cfg(test)]
mod normalize_tests {
    use super::normalize_rel;

    #[test]
    fn backslashes_become_forward_slashes() {
        assert_eq!(normalize_rel("a\\b\\c.txt"), "a/b/c.txt");
    }

    #[test]
    fn nfd_korean_normalizes_to_nfc() {
        use unicode_normalization::UnicodeNormalization;
        // macOS stores filenames decomposed (NFD); Windows/Linux use composed (NFC).
        // A decomposed Korean path must normalize back to the same NFC bytes so the
        // two filesystems agree on file identity.
        let nfc = "무제 폴더/파일.txt";
        let nfd: String = nfc.nfd().collect();
        assert_ne!(nfd, nfc, "precondition: NFD != NFC byte-wise");
        assert_eq!(normalize_rel(&nfd), nfc);
        assert_eq!(normalize_rel(nfc), nfc, "already-NFC is unchanged");
    }
}

/// Walk `root` recursively and build an index of every regular file.
pub fn build_index(root: &Path) -> Result<Index> {
    let mut index = Index::new();
    for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = match entry.path().strip_prefix(root) {
            Ok(p) => normalize_rel(&p.to_string_lossy()),
            Err(_) => continue,
        };
        if routing::is_minisync_internal(&rel) {
            continue;
        }
        if let Ok(fe) = entry_for(root, &rel) {
            index.insert(rel, fe);
        }
    }
    Ok(index)
}

/// Build a single [`FileEntry`] for the file at `root/rel`.
pub fn entry_for(root: &Path, rel: &str) -> Result<FileEntry> {
    let abs = root.join(rel);
    let meta = fs::metadata(&abs)?;
    let mtime = meta
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let bytes = fs::read(&abs)?;
    let hash = format!("{:x}", Sha256::digest(&bytes));
    let version = load_version(root, rel);
    Ok(FileEntry { path: rel.to_string(), size: meta.len(), mtime, hash, version })
}

// ── version vector ─────────────────────────────────────────────────────────

/// 버전벡터를 `.minisync/versions/<rel>.vv`에서 로드. 없으면 빈 맵.
pub fn load_version(root: &Path, rel: &str) -> HashMap<String, u64> {
    let p = routing::version_path(root, rel);
    match fs::read_to_string(&p) {
        Ok(content) => parse_vv(&content),
        Err(_) => HashMap::new(),
    }
}

/// 버전벡터를 디스크에 저장 (간단한 `key=value` 텍스트).
pub fn save_version(root: &Path, rel: &str, vv: &HashMap<String, u64>) {
    let p = routing::version_path(root, rel);
    if let Some(parent) = p.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let text: String = vv.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("\n");
    let _ = fs::write(&p, text);
}

fn parse_vv(s: &str) -> HashMap<String, u64> {
    let mut vv = HashMap::new();
    for line in s.lines() {
        if let Some((k, v)) = line.split_once('=') {
            if let Ok(n) = v.parse::<u64>() {
                vv.insert(k.to_string(), n);
            }
        }
    }
    vv
}

/// 두 버전벡터의 관계.
#[derive(Debug, PartialEq, Eq)]
pub enum VVRelation {
    Equal,
    /// a의 모든 카운터 ≥ b이고, 적어도 하나 >.
    ADominates,
    BDominates,
    /// 어느 쪽도 지배하지 않음 — 동시 수정.
    Concurrent,
}

pub fn compare_vv(a: &HashMap<String, u64>, b: &HashMap<String, u64>) -> VVRelation {
    let mut a_gt = false;
    let mut b_gt = false;
    let keys: std::collections::HashSet<&String> = a.keys().chain(b.keys()).collect();
    for key in keys {
        let va = a.get(key).copied().unwrap_or(0);
        let vb = b.get(key).copied().unwrap_or(0);
        if va > vb { a_gt = true; }
        if vb > va { b_gt = true; }
    }
    match (a_gt, b_gt) {
        (false, false) => VVRelation::Equal,
        (true, false) => VVRelation::ADominates,
        (false, true) => VVRelation::BDominates,
        (true, true) => VVRelation::Concurrent,
    }
}

/// 두 VV를 병합: 각 키의 최대값을 취한다.
pub fn merge_vv(a: &HashMap<String, u64>, b: &HashMap<String, u64>) -> HashMap<String, u64> {
    let mut result = a.clone();
    for (k, &v) in b {
        let entry = result.entry(k.clone()).or_insert(0);
        *entry = (*entry).max(v);
    }
    result
}
