//! Catalog: unified file view combining local and remote file entries.
//!
//! The catalog tracks every known file across all peers, whether the file
//! contents are stored locally, remotely (reference only), or both.
//! Each owner is identified by a `NodeInfo` (node_id + human-readable node_name).

pub mod store;

use crate::config::SyncMode;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Identifies a node in the network.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    pub node_id: String,
    pub node_name: String,
}

/// Where the file contents physically reside.
#[derive(Debug, Clone)]
pub enum FileLocation {
    /// File exists on the local disk only.
    Local,
    /// Only metadata is available; original is on remote peer(s).
    Remote { owners: Vec<NodeInfo> },
    /// File exists locally AND is known on remote peer(s).
    Both { owners: Vec<NodeInfo> },
}

/// A single entry in the unified catalog.
#[derive(Debug, Clone)]
pub struct CatalogEntry {
    pub path: String,
    pub size: u64,
    pub hash: String,
    pub location: FileLocation,
    pub sync_mode: SyncMode,
    /// The file's original creator (immutable). `None` until learned.
    pub origin: Option<NodeInfo>,
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
            origin: None,
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
        owner: NodeInfo,
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
            origin: None,
        });
        entry.size = size;
        entry.hash = hash;
        match &mut entry.location {
            FileLocation::Remote { owners } | FileLocation::Both { owners } => {
                if !owners.iter().any(|o| o.node_id == owner.node_id) {
                    owners.push(owner);
                } else {
                    // Update name if changed
                    if let Some(existing) = owners.iter_mut().find(|o| o.node_id == owner.node_id) {
                        existing.node_name = owner.node_name;
                    }
                }
            }
            FileLocation::Local => {
                entry.location = FileLocation::Both {
                    owners: vec![owner],
                };
            }
        }
    }

    /// Remove a file from the catalog.
    pub fn remove(&self, path: &str) {
        self.entries.write().unwrap().remove(path);
    }

    /// Last-known content hash of a path (for offline-edit detection on startup).
    pub fn hash_of(&self, path: &str) -> Option<String> {
        self.entries.read().unwrap().get(path).map(|e| e.hash.clone())
    }

    /// True if this path is already tracked as local (or both) with the given size.
    /// Used by the folder scanner to skip re-hashing unchanged files.
    pub fn is_local_with_size(&self, path: &str, size: u64) -> bool {
        let map = self.entries.read().unwrap();
        match map.get(path) {
            Some(e) => {
                e.size == size
                    && matches!(e.location, FileLocation::Local | FileLocation::Both { .. })
            }
            None => false,
        }
    }

    /// Reconcile the catalog's local view with what's actually on disk.
    /// `present` = relative paths currently present locally.
    /// Local-only entries that vanished are removed; Both entries that vanished
    /// downgrade to Remote (still downloadable); Remote-only entries are kept.
    pub fn reconcile_local(&self, present: &std::collections::HashSet<String>) {
        let mut map = self.entries.write().unwrap();
        map.retain(|path, entry| {
            if present.contains(path) {
                return true;
            }
            match &entry.location {
                FileLocation::Local => false,
                FileLocation::Both { owners } => {
                    entry.location = FileLocation::Remote {
                        owners: owners.clone(),
                    };
                    true
                }
                FileLocation::Remote { .. } => true,
            }
        });
    }

    /// Get a snapshot of all catalog entries (for GUI display).
    pub fn snapshot(&self) -> Vec<CatalogEntry> {
        let map = self.entries.read().unwrap();
        let mut entries: Vec<CatalogEntry> = map.values().cloned().collect();
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        entries
    }

    /// Get the owner node IDs of a remote/both file (for download routing).
    pub fn owners_of(&self, path: &str) -> Vec<String> {
        let map = self.entries.read().unwrap();
        match map.get(path) {
            Some(entry) => match &entry.location {
                FileLocation::Remote { owners } | FileLocation::Both { owners } => {
                    owners.iter().map(|o| o.node_id.clone()).collect()
                }
                FileLocation::Local => Vec::new(),
            },
            None => Vec::new(),
        }
    }

    /// User evicted the local copy (selective sync "Remove from this device").
    /// `Both` → `Remote` (keep it as a re-downloadable reference, peers untouched);
    /// `Local`-only (no remote owners) → removed from the catalog entirely.
    /// Returns true if a remote reference remains.
    pub fn evict_local(&self, path: &str) -> bool {
        let mut map = self.entries.write().unwrap();
        let owners = match map.get(path) {
            Some(e) => match &e.location {
                FileLocation::Both { owners } => owners.clone(),
                FileLocation::Remote { .. } => return true, // already a reference
                FileLocation::Local => {
                    map.remove(path);
                    return false;
                }
            },
            None => return false,
        };
        if let Some(e) = map.get_mut(path) {
            e.location = FileLocation::Remote { owners };
        }
        true
    }

    /// Record a file's origin (creator). Immutable in spirit: once set it only
    /// changes to a *smaller* node_id, so independent concurrent creations on
    /// different nodes converge to the same origin fleet-wide.
    pub fn set_origin(&self, path: &str, origin: NodeInfo) {
        let mut map = self.entries.write().unwrap();
        if let Some(entry) = map.get_mut(path) {
            let replace = match &entry.origin {
                None => true,
                Some(cur) => origin.node_id < cur.node_id,
            };
            if replace {
                entry.origin = Some(origin);
            }
        }
    }

    /// Add a remote holder (a peer that now has a local copy) to an existing
    /// entry. No-op if the path is unknown. `Local` → `Both`.
    pub fn add_holder(&self, path: &str, holder: NodeInfo) {
        let mut map = self.entries.write().unwrap();
        let Some(entry) = map.get_mut(path) else { return };
        match &mut entry.location {
            FileLocation::Remote { owners } | FileLocation::Both { owners } => {
                if let Some(existing) = owners.iter_mut().find(|o| o.node_id == holder.node_id) {
                    existing.node_name = holder.node_name;
                } else {
                    owners.push(holder);
                }
            }
            FileLocation::Local => {
                entry.location = FileLocation::Both {
                    owners: vec![holder],
                };
            }
        }
    }

    /// Remove a remote holder (a peer dropped its copy). `Both` stays `Both`/
    /// `Local` view of self is unaffected; `Remote` may end with zero holders.
    pub fn remove_holder(&self, path: &str, node_id: &str) {
        let mut map = self.entries.write().unwrap();
        let Some(entry) = map.get_mut(path) else { return };
        match &mut entry.location {
            FileLocation::Remote { owners } | FileLocation::Both { owners } => {
                owners.retain(|o| o.node_id != node_id);
            }
            FileLocation::Local => {}
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

    fn node(id: &str, name: &str) -> NodeInfo {
        NodeInfo {
            node_id: id.to_string(),
            node_name: name.to_string(),
        }
    }

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
            node("peer1", "WorkPC"),
            SyncMode::Reference,
        );
        let snap = cat.snapshot();
        assert_eq!(snap.len(), 1);
        match &snap[0].location {
            FileLocation::Remote { owners } => {
                assert_eq!(owners.len(), 1);
                assert_eq!(owners[0].node_id, "peer1");
                assert_eq!(owners[0].node_name, "WorkPC");
            }
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
            node("peer2", "HomeServer"),
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
            node("peer1", "WorkPC"),
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
        cat.upsert_remote("f.pdf".into(), 100, "h".into(), node("p1", "PC1"), SyncMode::Reference);
        cat.upsert_remote("f.pdf".into(), 100, "h".into(), node("p2", "PC2"), SyncMode::Reference);
        // Duplicate owner should not add twice
        cat.upsert_remote("f.pdf".into(), 100, "h".into(), node("p1", "PC1"), SyncMode::Reference);
        assert_eq!(cat.owners_of("f.pdf"), vec!["p1".to_string(), "p2".to_string()]);
    }

    #[test]
    fn evict_local_downgrades_both_to_remote() {
        let cat = Catalog::new();
        // File exists locally AND on a peer → Both.
        cat.upsert_local("doc.pdf".into(), 100, "h".into(), SyncMode::Reference);
        cat.upsert_remote("doc.pdf".into(), 100, "h".into(), node("p1", "PC1"), SyncMode::Reference);
        assert!(matches!(cat.snapshot()[0].location, FileLocation::Both { .. }));
        // Evict local copy → stays as a remote reference (peer untouched).
        assert!(cat.evict_local("doc.pdf"), "remote reference should remain");
        match &cat.snapshot()[0].location {
            FileLocation::Remote { owners } => assert_eq!(owners[0].node_id, "p1"),
            other => panic!("expected Remote, got {other:?}"),
        }
    }

    #[test]
    fn evict_local_removes_local_only() {
        let cat = Catalog::new();
        cat.upsert_local("mine.bin".into(), 10, "h".into(), SyncMode::Reference);
        // No remote owner → eviction removes it entirely, returns false.
        assert!(!cat.evict_local("mine.bin"));
        assert!(cat.snapshot().is_empty());
    }

    #[test]
    fn origin_is_immutable_smaller_id_wins() {
        let cat = Catalog::new();
        cat.upsert_remote("f".into(), 1, "h".into(), node("m", "Mid"), SyncMode::Reference);
        cat.set_origin("f", node("m", "Mid"));
        assert_eq!(cat.snapshot()[0].origin.as_ref().unwrap().node_id, "m");
        // A larger id must NOT override.
        cat.set_origin("f", node("z", "Zed"));
        assert_eq!(cat.snapshot()[0].origin.as_ref().unwrap().node_id, "m");
        // A smaller id wins (deterministic convergence for concurrent creation).
        cat.set_origin("f", node("a", "Ann"));
        assert_eq!(cat.snapshot()[0].origin.as_ref().unwrap().node_id, "a");
    }

    #[test]
    fn add_and_remove_holder() {
        let cat = Catalog::new();
        cat.upsert_local("f".into(), 1, "h".into(), SyncMode::FullCopy); // Local
        cat.add_holder("f", node("p1", "PC1")); // Local → Both
        assert!(matches!(cat.snapshot()[0].location, FileLocation::Both { .. }));
        assert_eq!(cat.owners_of("f"), vec!["p1".to_string()]);
        // A downloader that drops its copy is removed from holders.
        cat.remove_holder("f", "p1");
        assert!(cat.owners_of("f").is_empty());
        // add_holder on an unknown path is a no-op (doesn't create entries).
        cat.add_holder("ghost", node("p2", "PC2"));
        assert!(cat.owners_of("ghost").is_empty());
    }

    #[test]
    fn node_name_updated() {
        let cat = Catalog::new();
        cat.upsert_remote("f.pdf".into(), 100, "h".into(), node("p1", "OldName"), SyncMode::Reference);
        cat.upsert_remote("f.pdf".into(), 100, "h".into(), node("p1", "NewName"), SyncMode::Reference);
        let snap = cat.snapshot();
        match &snap[0].location {
            FileLocation::Remote { owners } => {
                assert_eq!(owners.len(), 1);
                assert_eq!(owners[0].node_name, "NewName");
            }
            _ => panic!("expected Remote"),
        }
    }
}
