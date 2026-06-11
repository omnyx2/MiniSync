//! Catalog JSON persistence: save/load catalog state to `.minisync/catalog.json`.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

use super::NodeInfo;
use crate::config::SyncMode;
use crate::routing;

/// Serializable catalog entry for JSON persistence.
#[derive(Debug, Serialize, Deserialize)]
struct StoredEntry {
    path: String,
    size: u64,
    hash: String,
    owners: Vec<NodeInfo>,
    is_local: bool,
    sync_mode: SyncMode,
}

/// Save the catalog state to disk.
pub fn save_catalog(root: &Path, catalog: &super::Catalog) {
    let entries = catalog.snapshot();
    let stored: Vec<StoredEntry> = entries
        .into_iter()
        .map(|e| {
            let (owners, is_local) = match &e.location {
                super::FileLocation::Local => (Vec::new(), true),
                super::FileLocation::Remote { owners } => (owners.clone(), false),
                super::FileLocation::Both { owners } => (owners.clone(), true),
            };
            StoredEntry {
                path: e.path,
                size: e.size,
                hash: e.hash,
                owners,
                is_local,
                sync_mode: e.sync_mode,
            }
        })
        .collect();

    let dir = root.join(routing::MINISYNC_DIR);
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("catalog.json");
    if let Ok(json) = serde_json::to_string_pretty(&stored) {
        let _ = fs::write(&path, json);
    }
}

/// Load catalog state from disk.
pub fn load_catalog(root: &Path, catalog: &super::Catalog) {
    let path = root.join(routing::MINISYNC_DIR).join("catalog.json");
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return,
    };
    let entries: Vec<StoredEntry> = match serde_json::from_str(&content) {
        Ok(e) => e,
        Err(_) => return,
    };

    for e in entries {
        if e.is_local {
            catalog.upsert_local(e.path.clone(), e.size, e.hash.clone(), e.sync_mode);
        }
        for owner in e.owners {
            catalog.upsert_remote(
                e.path.clone(),
                e.size,
                e.hash.clone(),
                owner,
                e.sync_mode,
            );
        }
    }
}
