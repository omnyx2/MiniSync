//! Catalog: unified file view combining local and remote file entries.
//!
//! The catalog tracks every known file across all peers, whether the file
//! contents are stored locally, remotely (reference only), or both.

pub mod store;

use crate::config::SyncMode;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Where the file contents physically reside.
#[derive(Debug, Clone)]
pub enum FileLocation {
    /// File exists on the local disk.
    Local,
    /// Only metadata is available; original is on remote peer(s).
    Remote { owners: Vec<String> },
    /// File exists locally AND is known on remote peer(s).
    Both { owners: Vec<String> },
}

/// A single entry in the unified catalog.
#[derive(Debug, Clone)]
pub struct CatalogEntry {
    pub path: String,
    pub size: u64,
    pub hash: String,
    pub location: FileLocation,
    pub sync_mode: SyncMode,
}

/// Thread-safe unified file catalog.
#[derive(Clone)]
pub struct Catalog {
    entries: Arc<RwLock<HashMap<String, CatalogEntry>>>,
}

impl Catalog {
    pub fn new() -> Self {
        Catalog {
            entries: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Update or insert a local file entry.
    pub fn upsert_local(&self, path: String, size: u64, hash: String, sync_mode: SyncMode) {
        let mut map = self.entries.write().unwrap();
        let entry = map.entry(path.clone()).or_insert_with(|| CatalogEntry {
            path: path.clone(),
            size,
            hash: hash.clone(),
            location: FileLocation::Local,
            sync_mode,
        });
        entry.size = size;
        entry.hash = hash;
        entry.sync_mode = sync_mode;
        match &entry.location {
            FileLocation::Remote { owners } => {
                entry.location = FileLocation::Both {
                    owners: owners.clone(),
                };
            }
            FileLocation::Local | FileLocation::Both { .. } => {
                // Already local
            }
        }
    }

    /// Update or insert a remote (reference) file entry.
    pub fn upsert_remote(
        &self,
        path: String,
        size: u64,
        hash: String,
        owner_id: String,
        sync_mode: SyncMode,
    ) {
        let mut map = self.entries.write().unwrap();
        let entry = map.entry(path.clone()).or_insert_with(|| CatalogEntry {
            path: path.clone(),
            size,
            hash: hash.clone(),
            location: FileLocation::Remote {
                owners: Vec::new(),
            },
            sync_mode,
        });
        entry.size = size;
        entry.hash = hash;
        match &mut entry.location {
            FileLocation::Remote { owners } | FileLocation::Both { owners } => {
                if !owners.contains(&owner_id) {
                    owners.push(owner_id);
                }
            }
            FileLocation::Local => {
                entry.location = FileLocation::Both {
                    owners: vec![owner_id],
                };
            }
        }
    }

    /// Remove a file from the catalog.
    pub fn remove(&self, path: &str) {
        self.entries.write().unwrap().remove(path);
    }

    /// Get a snapshot of all catalog entries (for GUI display).
    pub fn snapshot(&self) -> Vec<CatalogEntry> {
        let map = self.entries.read().unwrap();
        let mut entries: Vec<CatalogEntry> = map.values().cloned().collect();
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        entries
    }

    /// Get the owners of a remote/both file (for download routing).
    pub fn owners_of(&self, path: &str) -> Vec<String> {
        let map = self.entries.read().unwrap();
        match map.get(path) {
            Some(entry) => match &entry.location {
                FileLocation::Remote { owners } | FileLocation::Both { owners } => {
                    owners.clone()
                }
                FileLocation::Local => Vec::new(),
            },
            None => Vec::new(),
        }
    }

    /// Mark a file as now also local (after download).
    pub fn mark_local(&self, path: &str) {
        let mut map = self.entries.write().unwrap();
        if let Some(entry) = map.get_mut(path) {
            match &entry.location {
                FileLocation::Remote { owners } => {
                    entry.location = FileLocation::Both {
                        owners: owners.clone(),
                    };
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_upsert() {
        let cat = Catalog::new();
        cat.upsert_local(
            "file.txt".into(),
            100,
            "abc".into(),
            SyncMode::FullCopy,
        );
        let snap = cat.snapshot();
        assert_eq!(snap.len(), 1);
        assert!(matches!(snap[0].location, FileLocation::Local));
    }

    #[test]
    fn remote_upsert() {
        let cat = Catalog::new();
        cat.upsert_remote(
            "big.pdf".into(),
            1_000_000,
            "def".into(),
            "peer1".into(),
            SyncMode::Reference,
        );
        let snap = cat.snapshot();
        assert_eq!(snap.len(), 1);
        match &snap[0].location {
            FileLocation::Remote { owners } => assert_eq!(owners, &vec!["peer1".to_string()]),
            _ => panic!("expected Remote"),
        }
    }

    #[test]
    fn local_then_remote_becomes_both() {
        let cat = Catalog::new();
        cat.upsert_local("file.txt".into(), 100, "abc".into(), SyncMode::FullCopy);
        cat.upsert_remote(
            "file.txt".into(),
            100,
            "abc".into(),
            "peer2".into(),
            SyncMode::FullCopy,
        );
        let snap = cat.snapshot();
        assert!(matches!(snap[0].location, FileLocation::Both { .. }));
    }

    #[test]
    fn remote_then_local_becomes_both() {
        let cat = Catalog::new();
        cat.upsert_remote(
            "big.pdf".into(),
            1_000_000,
            "def".into(),
            "peer1".into(),
            SyncMode::Reference,
        );
        cat.upsert_local(
            "big.pdf".into(),
            1_000_000,
            "def".into(),
            SyncMode::Reference,
        );
        let snap = cat.snapshot();
        assert!(matches!(snap[0].location, FileLocation::Both { .. }));
    }

    #[test]
    fn owners_tracking() {
        let cat = Catalog::new();
        cat.upsert_remote("f.pdf".into(), 100, "h".into(), "p1".into(), SyncMode::Reference);
        cat.upsert_remote("f.pdf".into(), 100, "h".into(), "p2".into(), SyncMode::Reference);
        // Duplicate owner should not add twice
        cat.upsert_remote("f.pdf".into(), 100, "h".into(), "p1".into(), SyncMode::Reference);
        assert_eq!(cat.owners_of("f.pdf"), vec!["p1".to_string(), "p2".to_string()]);
    }
}
