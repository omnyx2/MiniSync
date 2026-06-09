//! The heart of the tool: keep N peers' folders in sync over a full mesh P2P.
//!
//! Per-peer thread (run_peer_session):
//!   - TLS handshake → Hello/Index 교환 → reader loop (수신 + 로컬 적용)
//!
//! 공유 watcher thread (watch_loop):
//!   - 로컬 파일 변경 감지 → PeerRegistry.broadcast()로 모든 피어에 전송
//!
//! Full mesh: 모든 노드가 직접 연결. relay 없음 — originator의 watcher만 broadcast.
//!
//! TLS: rustls 자체서명 인증서. TlsStream은 clone 불가 → Arc<Mutex> + 채널 기반 writer.
//!
//! 두 레인:
//!   - 파일 레인: File 메시지 + 버전벡터(동시수정 → .conflict 보존)
//!   - CRDT 레인: CrdtSync/CrdtChanges로 Automerge 연산 교환

use anyhow::{bail, Result};
use automerge::AutoCommit;
use rustls::{ClientConfig, ServerConfig};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::crdt;
use crate::index::{
    build_index, compare_vv, entry_for, merge_vv, save_version, FileEntry, VVRelation,
};
use crate::peers::{send_to_peer, PeerConn, PeerRegistry};
use crate::protocol::{recv_msg, send_msg, Message};
use crate::routing::{self, Lane};
use crate::tls::{self, TlsStream};
use crate::watcher::{watch_folder, WatchEvent};

pub type Seen = Arc<Mutex<HashMap<String, String>>>;
pub type CrdtDocs = Arc<Mutex<HashMap<String, AutoCommit>>>;

const DELETED_HASH: &str = "";

// ── per-peer session ──────────────────────────────────────────────────────

/// 한 피어의 전체 라이프사이클: TLS handshake → Hello → Index → reader loop.
pub fn run_peer_session(
    tcp: TcpStream,
    is_server: bool,
    server_cfg: Arc<ServerConfig>,
    client_cfg: Arc<ClientConfig>,
    registry: Arc<PeerRegistry>,
    root: Arc<PathBuf>,
    seen: Seen,
    docs: CrdtDocs,
    peer_id: String,
) -> Result<()> {
    // 1) TLS handshake
    let mut tls = if is_server {
        tls::accept_tls(tcp, server_cfg)?
    } else {
        tls::connect_tls(tcp, client_cfg)?
    };
    println!("[minisync] TLS handshake complete ({})", if is_server { "server" } else { "client" });

    // 2) Hello 교환 (blocking, no timeout yet)
    send_msg(&mut tls, &Message::Hello(peer_id.clone()))?;
    let remote_id = match recv_msg(&mut tls)? {
        Some(Message::Hello(id)) => id,
        _ => bail!("expected Hello from peer"),
    };
    println!("[minisync] remote peer: {remote_id}");

    // 3) Set short read timeout so reader doesn't hold mutex too long
    tls.set_read_timeout(Some(Duration::from_millis(1)))?;

    // 4) Wrap in Arc<Mutex> for sharing between reader and writer
    let stream = Arc::new(Mutex::new(tls));

    // 5) Create channel for writer
    let (tx, rx) = mpsc::channel::<Vec<u8>>();

    // 6) Atomic 등록 (이미 같은 peer_id가 있으면 거부)
    let (conn_id, peer_conn) = match registry.add_if_new(remote_id.clone(), tx) {
        Some(pair) => pair,
        None => {
            println!("[minisync] already connected to {remote_id}, closing duplicate");
            return Ok(());
        }
    };
    println!(
        "[minisync] registered conn_id={conn_id}, peers={}",
        registry.count()
    );

    // 7) Writer thread: channel → TLS write
    let stream_w = Arc::clone(&stream);
    let remote_id_w = remote_id.clone();
    let writer_handle = std::thread::spawn(move || {
        for bytes in rx {
            let mut guard = stream_w.lock().unwrap();
            if let Err(e) = guard.write_all(&bytes) {
                eprintln!("[writer] {remote_id_w}: {e}");
                break;
            }
            if let Err(e) = guard.flush() {
                eprintln!("[writer] {remote_id_w} flush: {e}");
                break;
            }
        }
    });

    // 8) Index 전송 (via channel)
    let entries: Vec<FileEntry> = build_index(&root)?.into_values().collect();
    send_to_peer(&peer_conn, &Message::Index(entries))?;

    // 9) Reader loop (manual buffering with timeout)
    let result = reader_loop_buffered(
        &stream,
        &peer_conn,
        &root,
        &seen,
        &docs,
        &peer_id,
        &remote_id,
    );

    // 10) 정리: registry에서 제거 → Sender drop → writer thread 종료
    registry.remove(conn_id);
    drop(peer_conn);
    let _ = writer_handle.join();
    println!(
        "[minisync] peer {remote_id} (conn_id={conn_id}) disconnected, peers={}",
        registry.count()
    );
    result
}

/// Manual buffered reader: lock → read (with timeout) → unlock → parse.
fn reader_loop_buffered(
    stream: &Arc<Mutex<TlsStream>>,
    peer_conn: &Arc<PeerConn>,
    root: &Path,
    seen: &Seen,
    docs: &CrdtDocs,
    peer_id: &str,
    remote_id: &str,
) -> Result<()> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 16384];

    loop {
        // 1) Lock → read → unlock (lock held only during read, max ~1ms)
        let read_result = {
            let mut guard = stream.lock().unwrap();
            guard.read(&mut tmp)
        }; // lock released immediately

        match read_result {
            Ok(0) => {
                println!("[sync] peer {remote_id} disconnected (EOF)");
                return Ok(());
            }
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // No data — sleep without lock to let writer thread work
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            Err(e) => {
                return Err(e.into());
            }
        }

        // 2) Parse complete messages from buffer
        while buf.len() >= 4 {
            let len =
                u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
            if buf.len() < 4 + len {
                break; // incomplete message, wait for more data
            }
            let msg: Message = bincode::deserialize(&buf[4..4 + len])?;
            buf.drain(..4 + len);
            handle_message(msg, peer_conn, root, seen, docs, peer_id, remote_id)?;
        }
    }
}

// ── message handling ───────────────────────────────────────────────────────

fn handle_message(
    msg: Message,
    peer_conn: &Arc<PeerConn>,
    root: &Path,
    seen: &Seen,
    docs: &CrdtDocs,
    peer_id: &str,
    remote_id: &str,
) -> Result<()> {
    match msg {
        Message::Hello(_) => {} // already handled
        Message::Index(entries) => {
            handle_index(entries, peer_conn, root, docs)?;
        }
        Message::Request(path) => {
            handle_request(&path, peer_conn, root, docs, peer_id)?;
        }
        Message::File { entry, contents } => {
            handle_file(entry, &contents, root, seen, remote_id, peer_id)?;
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
        }
        Message::CrdtSync { path, data } => {
            handle_crdt_sync(&path, &data, root, seen, docs, peer_conn)?;
        }
        Message::CrdtChanges { path, changes } => {
            handle_crdt_changes(&path, &changes, root, seen, docs, peer_conn)?;
        }
    }
    Ok(())
}

fn handle_index(
    entries: Vec<FileEntry>,
    peer_conn: &Arc<PeerConn>,
    root: &Path,
    docs: &CrdtDocs,
) -> Result<()> {
    let local = build_index(root)?;

    for e in &entries {
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
    let peer_paths: std::collections::HashSet<&str> =
        entries.iter().map(|e| e.path.as_str()).collect();
    for (rel, _) in &local {
        if routing::lane_for(rel) == Lane::Crdt && !peer_paths.contains(rel.as_str()) {
            let data = {
                let mut map = docs.lock().unwrap();
                let doc = map
                    .entry(rel.clone())
                    .or_insert_with(|| crdt::load_or_create_doc(root, rel));
                doc.save()
            };
            send_to_peer(peer_conn, &Message::CrdtSync { path: rel.clone(), data })?;
        }
    }

    // 우리만 가진 파일 레인 파일 전송
    for (rel, fe) in &local {
        if routing::lane_for(rel) == Lane::File && !peer_paths.contains(rel.as_str()) {
            if let Ok(contents) = fs::read(root.join(rel)) {
                send_to_peer(peer_conn, &Message::File { entry: fe.clone(), contents })?;
            }
        }
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
    remote_id: &str,
    peer_id: &str,
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
                }
                VVRelation::BDominates => {
                    println!("[sync] ignoring older version of {}", entry.path);
                }
                VVRelation::Concurrent => {
                    let merged = merge_vv(&entry.version, &local_vv);
                    save_version(root, &entry.path, &merged);

                    let conf_rel = conflict_path(&entry.path, remote_id);
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

// ── file helpers ───────────────────────────────────────────────────────────

fn apply_file(root: &Path, entry: &FileEntry, contents: &[u8]) -> Result<()> {
    let dest = root.join(&entry.path);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&dest, contents)?;
    Ok(())
}

fn sha256_hex(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

/// `report.pdf` + peer `a1b2` → `report.conflict-a1b2.pdf`
fn conflict_path(rel: &str, peer_id: &str) -> String {
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

// ── watch loop ─────────────────────────────────────────────────────────────

/// 로컬 파일 변경 감시 → PeerRegistry.broadcast()로 모든 피어에 전송.
pub fn watch_loop(
    registry: Arc<PeerRegistry>,
    root: Arc<PathBuf>,
    seen: Seen,
    docs: CrdtDocs,
    peer_id: String,
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

                match routing::lane_for(&rel) {
                    Lane::Crdt => {
                        handle_crdt_local_edit(&rel, &entry, &registry, &root, &seen, &docs)?;
                    }
                    Lane::File => {
                        let mut vv = entry.version.clone();
                        *vv.entry(peer_id.clone()).or_insert(0) += 1;
                        save_version(&root, &rel, &vv);

                        let mut entry = entry;
                        entry.version = vv;
                        seen.lock().unwrap().insert(rel.clone(), entry.hash.clone());
                        let contents = fs::read(root.join(&rel))?;
                        println!("[watch] sending {rel}");
                        registry.broadcast(&Message::File { entry, contents });
                    }
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
                seen.lock().unwrap().insert(rel.clone(), DELETED_HASH.to_string());
                println!("[watch] sending delete {rel}");
                registry.broadcast(&Message::Delete(rel));
            }
        }
    }
    Ok(())
}

fn handle_crdt_local_edit(
    rel: &str,
    entry: &FileEntry,
    registry: &Arc<PeerRegistry>,
    root: &Path,
    seen: &Seen,
    docs: &CrdtDocs,
) -> Result<()> {
    let content = fs::read_to_string(root.join(rel))?;
    let shadow = crdt::read_shadow(root, rel);

    if content == shadow {
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
