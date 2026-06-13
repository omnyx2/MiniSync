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
use crate::index::{entry_for, load_version, save_version};
use crate::routing::{self, Lane};

/// One-shot at startup (BEFORE the scan loop / before connecting): detect files
/// edited while we were OFF — disk hash differs from the last-known catalog hash —
/// and bump our version-vector counter so the change is causally tracked. Without
/// this, two nodes that both edited the same binary offline have equal version
/// vectors, so it resolves by mtime and one copy is silently overwritten. With it,
/// the edits are concurrent → a `.conflict-<peer>` copy is kept instead.
///
/// CRDT/text files are skipped: their shadow-diff already captures offline edits
/// and merges them losslessly.
pub fn reconcile_offline_edits(root: &std::path::Path, catalog: &Catalog, peer_id: &str) {
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = match entry.path().strip_prefix(root) {
            Ok(p) => crate::index::normalize_rel(&p.to_string_lossy()),
            Err(_) => continue,
        };
        if routing::is_minisync_internal(&rel) || routing::lane_for(&rel) != Lane::File {
            continue;
        }
        let Some(known) = catalog.hash_of(&rel) else {
            continue; // not previously tracked → a new file, handled normally
        };
        let cur = match entry_for(root, &rel) {
            Ok(fe) => fe.hash,
            Err(_) => continue,
        };
        if known != cur {
            let mut vv = load_version(root, &rel);
            *vv.entry(peer_id.to_string()).or_insert(0) += 1;
            save_version(root, &rel, &vv);
            println!("[startup] offline edit detected for {rel} — version bumped");
        }
    }
}

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
    let self_node = engine.as_ref().map(|e| crate::catalog::NodeInfo {
        node_id: e.peer_id.clone(),
        node_name: e.node_name.clone(),
    });
    loop {
        scan_once(&root, &catalog, &config, self_node.as_ref());
        // 디스크 변화를 GUI가 바로 반영하도록 알림 + 영속화.
        if let Some(eng) = &engine {
            eng.notify_gui(EngineEvent::CatalogUpdated);
        }
        store::save_catalog(&root, &catalog);
        std::thread::sleep(SCAN_INTERVAL);
    }
}

/// 한 번 스캔: 현재 존재하는 파일을 카탈로그에 반영하고 사라진 것을 정리.
fn scan_once(
    root: &PathBuf,
    catalog: &Catalog,
    config: &Arc<RwLock<SyncConfig>>,
    self_node: Option<&crate::catalog::NodeInfo>,
) {
    let mut present: HashSet<String> = HashSet::new();

    for entry in walkdir::WalkDir::new(root.as_path())
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = match entry.path().strip_prefix(root.as_path()) {
            // Same NFC normalization as build_index/watch so a Korean filename
            // doesn't get a divergent NFD catalog key (which would duplicate rows).
            Ok(p) => crate::index::normalize_rel(&p.to_string_lossy()),
            Err(_) => continue,
        };
        if routing::is_minisync_internal(&rel) {
            continue;
        }
        present.insert(rel.clone());

        // Origin reconciliation — runs even when the content-change check below
        // skips re-hashing, so a file cataloged before its origin was known still
        // gets it. Adopt the recorded origin, or stamp ourselves for a held file
        // that has none (we introduced it to the mesh).
        match crate::index::load_origin(root, &rel) {
            Some(o) => catalog.set_origin(&rel, o),
            None => {
                if let Some(me) = self_node {
                    crate::index::save_origin(root, &rel, me);
                    catalog.set_origin(&rel, me.clone());
                }
            }
        }

        // CRDT(텍스트) 파일은 내용이 같은 지금(편집 전) 문서를 선제 생성해 둔다.
        // 두 피어가 동일 내용에서 결정적 genesis로 만들면 같은 계보가 되어,
        // 이후 첫 동시 편집도 데이터 유실 없이 병합된다.
        if routing::lane_for(&rel) == routing::Lane::Crdt {
            crate::crdt::ensure_doc(root, &rel);
        }

        // 크기가 그대로면 이미 표시 중이므로 재해시 생략(비용 절약).
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        if catalog.is_local_with_size(&rel, size) {
            continue;
        }
        // 신규/변경/원격→로컬 전환 파일만 해시하여 반영.
        if let Ok(fe) = entry_for(root, &rel) {
            let mode = config.read().unwrap().mode_for(&rel);
            catalog.upsert_local(rel.clone(), fe.size, fe.hash, mode);
            // Entry now exists — make the origin stick immediately.
            if let Some(o) = crate::index::load_origin(root, &rel) {
                catalog.set_origin(&rel, o);
            }
        }
    }

    catalog.reconcile_local(&present);
}
