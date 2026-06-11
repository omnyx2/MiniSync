//! Catalog scanner: periodically reconcile the unified catalog with what's
//! actually on disk, so the GUI shows every file currently in the sync folder.
//!
//! The catalog is otherwise only updated incrementally on sync events
//! (`handle_file`, watcher edits), and CRDT-lane files / pre-existing files
//! were never recorded — so files synced in a previous session (or via the
//! CRDT lane) stayed invisible. This loop walks the folder, upserts present
//! files as local entries, prunes ones that vanished, and persists the catalog.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use super::{EngineEvent, SyncEngine};
use crate::catalog::{store, Catalog};
use crate::config::SyncConfig;
use crate::index::entry_for;
use crate::routing;

/// 스캔 주기. 너무 짧으면 디스크 부담, 너무 길면 갱신이 느리다.
const SCAN_INTERVAL: Duration = Duration::from_secs(2);

/// 공유 폴더를 주기적으로 스캔해 카탈로그를 디스크 상태와 정합화한다.
/// (전용 스레드에서 무한 루프)
pub fn catalog_scan_loop(
    root: Arc<PathBuf>,
    catalog: Catalog,
    config: Arc<RwLock<SyncConfig>>,
    engine: Option<Arc<SyncEngine>>,
) {
    loop {
        scan_once(&root, &catalog, &config);
        // 디스크 변화를 GUI가 바로 반영하도록 알림 + 영속화.
        if let Some(eng) = &engine {
            eng.notify_gui(EngineEvent::CatalogUpdated);
        }
        store::save_catalog(&root, &catalog);
        std::thread::sleep(SCAN_INTERVAL);
    }
}

/// 한 번 스캔: 현재 존재하는 파일을 카탈로그에 반영하고 사라진 것을 정리.
fn scan_once(root: &PathBuf, catalog: &Catalog, config: &Arc<RwLock<SyncConfig>>) {
    let mut present: HashSet<String> = HashSet::new();

    for entry in walkdir::WalkDir::new(root.as_path())
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = match entry.path().strip_prefix(root.as_path()) {
            Ok(p) => p.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        if routing::is_minisync_internal(&rel) {
            continue;
        }
        present.insert(rel.clone());

        // 크기가 그대로면 이미 표시 중이므로 재해시 생략(비용 절약).
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        if catalog.is_local_with_size(&rel, size) {
            continue;
        }
        // 신규/변경/원격→로컬 전환 파일만 해시하여 반영.
        if let Ok(fe) = entry_for(root, &rel) {
            let mode = config.read().unwrap().mode_for(&rel);
            catalog.upsert_local(rel, fe.size, fe.hash, mode);
        }
    }

    catalog.reconcile_local(&present);
}
