//! Message dispatch and individual handlers for each message type.

use anyhow::Result;
use automerge::AutoCommit;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use super::transfer::{apply_file, conflict_path, sha256_hex};
use super::{CrdtDocs, EngineEvent, Seen, SyncEngine, DELETED_HASH};
use crate::catalog::{Catalog, NodeInfo};
use crate::config::{SyncConfig, SyncMode};
use crate::crdt;
use crate::index::{
    build_index, compare_vv, entry_for, merge_vv, save_version, FileEntry, VVRelation,
};
use crate::net::peers::{send_to_peer, PeerConn};
use crate::protocol::{Message, RefEntry};
use crate::routing::{self, Lane};
use std::sync::RwLock;

/// Dispatch an incoming message to the appropriate handler.
pub fn handle_message(
    msg: Message,
    peer_conn: &Arc<PeerConn>,
    root: &Path,
    seen: &Seen,
    docs: &CrdtDocs,
    peer_id: &str,
    node_name: &str,
    remote_id: &str,
    remote_name: &str,
    config: &Arc<RwLock<SyncConfig>>,
    catalog: &Catalog,
    engine: Option<&SyncEngine>,
) -> Result<()> {
    let remote_node = NodeInfo {
        node_id: remote_id.to_string(),
        node_name: remote_name.to_string(),
    };
    match msg {
        Message::Hello { .. } => {} // already handled
        Message::Index(entries) => {
            handle_index(entries, peer_conn, root, docs, config, catalog, peer_id, node_name, &remote_node, engine)?;
        }
        Message::Request(path) => {
            handle_request(&path, peer_conn, root, docs, peer_id)?;
        }
        Message::File { entry, contents } => {
            handle_file(entry, &contents, root, seen, &remote_node, peer_id, config, catalog, engine)?;
        }
        Message::Delete(ref path) => {
            seen.lock()
                .unwrap()
                .insert(path.clone(), DELETED_HASH.to_string());
            let target = root.join(path);
            if target.exists() {
                fs::remove_file(&target)?;
                println!("[sync] deleted {path}");
            }
            catalog.remove(path);
            if let Some(eng) = engine {
                eng.notify_gui(EngineEvent::CatalogUpdated);
            }
        }
        Message::CrdtSync { path, data } => {
            handle_crdt_sync(&path, &data, root, seen, docs, peer_conn)?;
            // CRDT files are always FullCopy
            let size = fs::metadata(root.join(&path)).map(|m| m.len()).unwrap_or(0);
            let hash = seen.lock().unwrap().get(&path).cloned().unwrap_or_default();
            catalog.upsert_local(path, size, hash, SyncMode::FullCopy);
            if let Some(eng) = engine {
                eng.notify_gui(EngineEvent::CatalogUpdated);
            }
        }
        Message::CrdtChanges { path, changes } => {
            handle_crdt_changes(&path, &changes, root, seen, docs, peer_conn)?;
            let size = fs::metadata(root.join(&path)).map(|m| m.len()).unwrap_or(0);
            let hash = seen.lock().unwrap().get(&path).cloned().unwrap_or_default();
            catalog.upsert_local(path, size, hash, SyncMode::FullCopy);
            if let Some(eng) = engine {
                eng.notify_gui(EngineEvent::CatalogUpdated);
            }
        }
        Message::RefIndex(ref_entries) => {
            handle_ref_index(ref_entries, catalog, engine)?;
        }
        Message::DownloadRequest(path) => {
            handle_download_request(&path, peer_conn, root, peer_id)?;
        }
    }
    Ok(())
}

fn handle_index(
    entries: Vec<FileEntry>,
    peer_conn: &Arc<PeerConn>,
    root: &Path,
    docs: &CrdtDocs,
    config: &Arc<RwLock<SyncConfig>>,
    catalog: &Catalog,
    peer_id: &str,
    node_name: &str,
    remote_node: &NodeInfo,
    engine: Option<&SyncEngine>,
) -> Result<()> {
    let local = build_index(root)?;
    let cfg = config.read().unwrap().clone();

    for e in &entries {
        // Check if this file is in reference mode for the remote peer
        let mode = cfg.mode_for(&e.path);

        // For reference-mode files, just update catalog metadata
        if mode == SyncMode::Reference && routing::lane_for(&e.path) != Lane::Crdt {
            catalog.upsert_remote(
                e.path.clone(),
                e.size,
                e.hash.clone(),
                remote_node.clone(),
                SyncMode::Reference,
            );
            continue;
        }

        let need = match local.get(&e.path) {
            None => true,
            Some(mine) => {
                if mine.hash == e.hash {
                    false
                } else {
                    match compare_vv(&e.version, &mine.version) {
                        VVRelation::ADominates | VVRelation::Concurrent => true,
                        VVRelation::Equal => e.mtime > mine.mtime,
                        VVRelation::BDominates => false,
                    }
                }
            }
        };
        if need {
            send_to_peer(peer_conn, &Message::Request(e.path.clone()))?;
        }
    }

    // 우리만 가진 CRDT 파일 전송
    let peer_paths: HashSet<&str> = entries.iter().map(|e| e.path.as_str()).collect();
    for (rel, _) in &local {
        if routing::lane_for(rel) == Lane::Crdt && !peer_paths.contains(rel.as_str()) {
            let data = {
                let mut map = docs.lock().unwrap();
                let doc = map
                    .entry(rel.clone())
                    .or_insert_with(|| crdt::load_or_create_doc(root, rel));
                doc.save()
            };
            send_to_peer(
                peer_conn,
                &Message::CrdtSync {
                    path: rel.clone(),
                    data,
                },
            )?;
        }
    }

    // 우리만 가진 파일 레인 파일 전송 (FullCopy) or RefIndex (Reference)
    let mut ref_entries_to_send = Vec::new();
    for (rel, fe) in &local {
        if routing::lane_for(rel) == Lane::File && !peer_paths.contains(rel.as_str()) {
            let mode = cfg.mode_for(rel);
            match mode {
                SyncMode::FullCopy => {
                    if let Ok(contents) = fs::read(root.join(rel)) {
                        send_to_peer(
                            peer_conn,
                            &Message::File {
                                entry: fe.clone(),
                                contents,
                            },
                        )?;
                    }
                }
                SyncMode::Reference => {
                    ref_entries_to_send.push(RefEntry {
                        path: rel.clone(),
                        size: fe.size,
                        hash: fe.hash.clone(),
                        mtime: fe.mtime,
                        owner_id: peer_id.to_string(),
                        owner_name: node_name.to_string(),
                    });
                }
            }
        }
    }
    if !ref_entries_to_send.is_empty() {
        send_to_peer(peer_conn, &Message::RefIndex(ref_entries_to_send))?;
    }

    if let Some(eng) = engine {
        eng.notify_gui(EngineEvent::CatalogUpdated);
    }

    Ok(())
}

fn handle_request(
    path: &str,
    peer_conn: &Arc<PeerConn>,
    root: &Path,
    docs: &CrdtDocs,
    peer_id: &str,
) -> Result<()> {
    match routing::lane_for(path) {
        Lane::Crdt => {
            let data = {
                let mut map = docs.lock().unwrap();
                let doc = map
                    .entry(path.to_string())
                    .or_insert_with(|| crdt::load_or_create_doc(root, path));
                doc.save()
            };
            send_to_peer(
                peer_conn,
                &Message::CrdtSync {
                    path: path.to_string(),
                    data,
                },
            )?;
        }
        Lane::File => {
            if let Ok(mut entry) = entry_for(root, path) {
                if entry.version.is_empty() {
                    entry.version.insert(peer_id.to_string(), 1);
                    save_version(root, path, &entry.version);
                }
                let contents = fs::read(root.join(path))?;
                send_to_peer(peer_conn, &Message::File { entry, contents })?;
            }
        }
    }
    Ok(())
}

/// 파일 레인 수신: 버전벡터로 최신/동시수정 판별.
fn handle_file(
    entry: FileEntry,
    contents: &[u8],
    root: &Path,
    seen: &Seen,
    remote_node: &NodeInfo,
    peer_id: &str,
    config: &Arc<RwLock<SyncConfig>>,
    catalog: &Catalog,
    engine: Option<&SyncEngine>,
) -> Result<()> {
    let local = entry_for(root, &entry.path).ok();

    match local {
        None => {
            seen.lock()
                .unwrap()
                .insert(entry.path.clone(), entry.hash.clone());
            save_version(root, &entry.path, &entry.version);
            apply_file(root, &entry, contents)?;
            println!("[sync] received {} ({} bytes)", entry.path, contents.len());

            let mode = config.read().unwrap().mode_for(&entry.path);
            catalog.upsert_local(entry.path, entry.size, entry.hash, mode);
            if let Some(eng) = engine {
                eng.notify_gui(EngineEvent::CatalogUpdated);
            }
        }
        Some(mine) => {
            if mine.hash == entry.hash {
                let merged = merge_vv(&entry.version, &mine.version);
                save_version(root, &entry.path, &merged);
                return Ok(());
            }

            let has_pending_local_edit = {
                let s = seen.lock().unwrap();
                match s.get(&entry.path) {
                    Some(h) => *h != mine.hash,
                    None => false,
                }
            };

            let local_vv = if has_pending_local_edit {
                let mut vv = mine.version.clone();
                *vv.entry(peer_id.to_string()).or_insert(0) += 1;
                save_version(root, &entry.path, &vv);
                vv
            } else {
                mine.version.clone()
            };

            match compare_vv(&entry.version, &local_vv) {
                VVRelation::ADominates | VVRelation::Equal => {
                    seen.lock()
                        .unwrap()
                        .insert(entry.path.clone(), entry.hash.clone());
                    save_version(root, &entry.path, &entry.version);
                    apply_file(root, &entry, contents)?;
                    println!("[sync] received {} ({} bytes)", entry.path, contents.len());

                    let mode = config.read().unwrap().mode_for(&entry.path);
                    catalog.upsert_local(
                        entry.path,
                        entry.size,
                        entry.hash,
                        mode,
                    );
                    if let Some(eng) = engine {
                        eng.notify_gui(EngineEvent::CatalogUpdated);
                    }
                }
                VVRelation::BDominates => {
                    println!("[sync] ignoring older version of {}", entry.path);
                }
                VVRelation::Concurrent => {
                    let merged = merge_vv(&entry.version, &local_vv);
                    save_version(root, &entry.path, &merged);

                    let conf_rel = conflict_path(&entry.path, &remote_node.node_id);
                    let conf_abs = root.join(&conf_rel);
                    if let Some(p) = conf_abs.parent() {
                        fs::create_dir_all(p)?;
                    }
                    fs::write(&conf_abs, contents)?;
                    seen.lock()
                        .unwrap()
                        .insert(conf_rel.clone(), entry.hash.clone());
                    println!(
                        "[sync] CONFLICT {} — remote saved as {}",
                        entry.path, conf_rel
                    );
                }
            }
        }
    }
    Ok(())
}

/// Handle incoming reference index entries — update catalog with remote metadata.
fn handle_ref_index(
    ref_entries: Vec<RefEntry>,
    catalog: &Catalog,
    engine: Option<&SyncEngine>,
) -> Result<()> {
    for re in ref_entries {
        println!(
            "[sync] received ref metadata: {} ({} bytes) from {} ({})",
            re.path, re.size, re.owner_name, re.owner_id
        );
        catalog.upsert_remote(
            re.path,
            re.size,
            re.hash,
            NodeInfo {
                node_id: re.owner_id,
                node_name: re.owner_name,
            },
            SyncMode::Reference,
        );
    }
    if let Some(eng) = engine {
        eng.notify_gui(EngineEvent::CatalogUpdated);
    }
    Ok(())
}

/// Handle a download request from a peer — send the file contents.
fn handle_download_request(
    path: &str,
    peer_conn: &Arc<PeerConn>,
    root: &Path,
    peer_id: &str,
) -> Result<()> {
    println!("[sync] download request for {path}");
    if let Ok(mut entry) = entry_for(root, path) {
        if entry.version.is_empty() {
            entry.version.insert(peer_id.to_string(), 1);
            save_version(root, path, &entry.version);
        }
        let contents = fs::read(root.join(path))?;
        send_to_peer(peer_conn, &Message::File { entry, contents })?;
    }
    Ok(())
}

// ── CRDT handlers ──────────────────────────────────────────────────────────

fn handle_crdt_sync(
    path: &str,
    data: &[u8],
    root: &Path,
    seen: &Seen,
    docs: &CrdtDocs,
    peer_conn: &Arc<PeerConn>,
) -> Result<()> {
    let mut doc = AutoCommit::load(data)?;
    let received_text = crdt::doc_text(&doc);

    let current_file = fs::read_to_string(root.join(path)).unwrap_or_default();
    let mut local_changes = Vec::new();
    if !current_file.is_empty() && current_file != received_text {
        crdt::apply_local_edit(&mut doc, &received_text, &current_file);
        local_changes = doc.save_incremental();
    }

    let content = crdt::doc_text(&doc);
    let hash = sha256_hex(content.as_bytes());
    seen.lock().unwrap().insert(path.to_string(), hash);

    let dest = root.join(path);
    if let Some(p) = dest.parent() {
        fs::create_dir_all(p)?;
    }
    fs::write(&dest, &content)?;
    crdt::save_doc_to_disk(root, path, &mut doc);
    crdt::write_shadow(root, path, &content);
    docs.lock().unwrap().insert(path.to_string(), doc);

    println!("[sync] received CRDT sync for {path}");

    if !local_changes.is_empty() {
        println!("[sync] sending captured local CRDT changes for {path}");
        send_to_peer(
            peer_conn,
            &Message::CrdtChanges {
                path: path.to_string(),
                changes: local_changes,
            },
        )?;
    }
    Ok(())
}

fn handle_crdt_changes(
    path: &str,
    changes: &[u8],
    root: &Path,
    seen: &Seen,
    docs: &CrdtDocs,
    peer_conn: &Arc<PeerConn>,
) -> Result<()> {
    let (content, local_changes) = {
        let mut map = docs.lock().unwrap();
        let doc = map
            .entry(path.to_string())
            .or_insert_with(|| crdt::load_or_create_doc(root, path));

        let shadow = crdt::read_shadow(root, path);
        let current_file = fs::read_to_string(root.join(path)).unwrap_or_default();
        let mut local_ch = Vec::new();
        if !current_file.is_empty() && current_file != shadow {
            crdt::apply_local_edit(doc, &shadow, &current_file);
            local_ch = doc.save_incremental();
        }

        doc.load_incremental(changes)?;
        let content = crdt::doc_text(doc);
        crdt::save_doc_to_disk(root, path, doc);
        (content, local_ch)
    };

    let hash = sha256_hex(content.as_bytes());
    seen.lock().unwrap().insert(path.to_string(), hash);

    let dest = root.join(path);
    if let Some(p) = dest.parent() {
        fs::create_dir_all(p)?;
    }
    fs::write(&dest, &content)?;
    crdt::write_shadow(root, path, &content);

    println!("[sync] applied CRDT changes for {path}");

    if !local_changes.is_empty() {
        println!("[sync] sending captured local CRDT changes for {path}");
        send_to_peer(
            peer_conn,
            &Message::CrdtChanges {
                path: path.to_string(),
                changes: local_changes,
            },
        )?;
    }
    Ok(())
}
