//! Watch loop: monitor local file changes and broadcast to all peers.

use anyhow::Result;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use super::{CrdtDocs, EngineEvent, Seen, SyncEngine, DELETED_HASH};
use crate::catalog::Catalog;
use crate::config::{SyncConfig, SyncMode};
use crate::crdt;
use crate::index::{entry_for, save_version, FileEntry};
use crate::net::peers::PeerRegistry;
use crate::protocol::{Message, RefEntry};
use crate::routing::{self, Lane};
use crate::watcher::{watch_folder, WatchEvent};

/// 로컬 파일 변경 감시 → PeerRegistry.broadcast()로 모든 피어에 전송.
pub fn watch_loop(
    registry: Arc<PeerRegistry>,
    root: Arc<PathBuf>,
    seen: Seen,
    docs: CrdtDocs,
    peer_id: String,
    node_name: String,
    config: Arc<RwLock<SyncConfig>>,
    catalog: Catalog,
    engine: Option<Arc<SyncEngine>>,
) -> Result<()> {
    let rx = watch_folder(&root)?;
    for event in rx {
        match event {
            WatchEvent::Changed(changed) => {
                let rel = match changed.strip_prefix(&*root) {
                    Ok(p) => p.to_string_lossy().replace('\\', "/"),
                    Err(_) => continue,
                };
                if rel.is_empty() || routing::is_minisync_internal(&rel) {
                    continue;
                }
                let entry = match entry_for(&root, &rel) {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                if seen.lock().unwrap().get(&rel) == Some(&entry.hash) {
                    continue;
                }

                let lane = routing::lane_for(&rel);
                let mode = config.read().unwrap().mode_for(&rel);

                // CRDT files are always FullCopy regardless of config
                if lane == Lane::Crdt {
                    handle_crdt_local_edit(
                        &rel, &entry, &registry, &root, &seen, &docs,
                    )?;
                    catalog.upsert_local(
                        rel.clone(),
                        entry.size,
                        entry.hash.clone(),
                        SyncMode::FullCopy,
                    );
                } else {
                    match mode {
                        SyncMode::FullCopy => {
                            let mut vv = entry.version.clone();
                            *vv.entry(peer_id.clone()).or_insert(0) += 1;
                            save_version(&root, &rel, &vv);

                            let mut entry = entry.clone();
                            entry.version = vv;
                            seen.lock().unwrap().insert(rel.clone(), entry.hash.clone());
                            let contents = fs::read(root.join(&rel))?;
                            println!("[watch] sending {rel}");
                            registry.broadcast(&Message::File { entry: entry.clone(), contents });

                            catalog.upsert_local(
                                rel.clone(),
                                entry.size,
                                entry.hash.clone(),
                                SyncMode::FullCopy,
                            );
                        }
                        SyncMode::Reference => {
                            // Only send metadata, not file contents
                            seen.lock().unwrap().insert(rel.clone(), entry.hash.clone());
                            let ref_entry = RefEntry {
                                path: rel.clone(),
                                size: entry.size,
                                hash: entry.hash.clone(),
                                mtime: entry.mtime,
                                owner_id: peer_id.clone(),
                                owner_name: node_name.clone(),
                            };
                            println!("[watch] sending ref metadata for {rel}");
                            registry.broadcast(&Message::RefIndex(vec![ref_entry]));

                            catalog.upsert_local(
                                rel.clone(),
                                entry.size,
                                entry.hash.clone(),
                                SyncMode::Reference,
                            );
                        }
                    }
                }

                if let Some(eng) = &engine {
                    eng.notify_gui(EngineEvent::CatalogUpdated);
                }
            }
            WatchEvent::Removed(removed) => {
                let rel = match removed.strip_prefix(&*root) {
                    Ok(p) => p.to_string_lossy().replace('\\', "/"),
                    Err(_) => continue,
                };
                if rel.is_empty() || routing::is_minisync_internal(&rel) {
                    continue;
                }
                {
                    let s = seen.lock().unwrap();
                    if s.get(&rel).map(|h| h.as_str()) == Some(DELETED_HASH) {
                        continue;
                    }
                }
                seen.lock()
                    .unwrap()
                    .insert(rel.clone(), DELETED_HASH.to_string());
                println!("[watch] sending delete {rel}");
                registry.broadcast(&Message::Delete(rel.clone()));

                catalog.remove(&rel);
                if let Some(eng) = &engine {
                    eng.notify_gui(EngineEvent::CatalogUpdated);
                }
            }
        }
    }
    Ok(())
}

fn handle_crdt_local_edit(
    rel: &str,
    entry: &FileEntry,
    registry: &Arc<PeerRegistry>,
    root: &std::path::Path,
    seen: &Seen,
    docs: &CrdtDocs,
) -> Result<()> {
    let content = fs::read_to_string(root.join(rel))?;
    let shadow = crdt::read_shadow(root, rel);

    // shadow가 이미 존재하고 내용이 같으면 진짜 "변화 없음" → skip.
    // shadow가 없으면(처음 보는 파일) 빈 파일이어도 초기 CrdtSync를 보내야 한다.
    // (read_shadow는 없는 shadow에 ""를 돌려주므로, 빈 파일이면 content==shadow가
    //  되어 동기화가 누락되는 버그를 막는다.)
    let shadow_exists = routing::shadow_path(root, rel).exists();
    if shadow_exists && content == shadow {
        seen.lock()
            .unwrap()
            .insert(rel.to_string(), entry.hash.clone());
        return Ok(());
    }

    let has_doc = docs.lock().unwrap().contains_key(rel);

    if has_doc {
        let changes = {
            let mut map = docs.lock().unwrap();
            let doc = map.get_mut(rel).unwrap();
            crdt::apply_local_edit(doc, &shadow, &content);
            let ch = doc.save_incremental();
            crdt::save_doc_to_disk(root, rel, doc);
            ch
        };
        crdt::write_shadow(root, rel, &content);
        seen.lock()
            .unwrap()
            .insert(rel.to_string(), entry.hash.clone());
        if !changes.is_empty() {
            println!("[watch] sending CRDT changes for {rel}");
            registry.broadcast(&Message::CrdtChanges {
                path: rel.to_string(),
                changes,
            });
        }
    } else {
        let mut doc = crdt::new_doc(&content);
        let data = doc.save();
        crdt::save_doc_to_disk(root, rel, &mut doc);
        crdt::write_shadow(root, rel, &content);
        seen.lock()
            .unwrap()
            .insert(rel.to_string(), entry.hash.clone());
        docs.lock().unwrap().insert(rel.to_string(), doc);
        println!("[watch] sending CRDT sync for {rel}");
        registry.broadcast(&Message::CrdtSync {
            path: rel.to_string(),
            data,
        });
    }
    Ok(())
}
