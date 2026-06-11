//! Per-peer session lifecycle: TLS handshake → Hello → Index → reader loop.

use anyhow::{bail, Result};
use rustls::{ClientConfig, ServerConfig};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use super::handlers::handle_message;
use super::{CrdtDocs, EngineEvent, Seen, SyncEngine};
use crate::catalog::Catalog;
use crate::config::SyncConfig;
use crate::index::{build_index, FileEntry};
use crate::net::peers::{send_to_peer, PeerConn, PeerRegistry};
use crate::net::{self, TlsStream};
use crate::protocol::{recv_msg, send_msg, Message};

/// 한 피어의 전체 라이프사이클: TLS handshake → Hello → Index → reader loop.
pub fn run_peer_session(
    tcp: TcpStream,
    is_server: bool,
    server_cfg: Arc<ServerConfig>,
    client_cfg: Arc<ClientConfig>,
    registry: Arc<PeerRegistry>,
    root: Arc<std::path::PathBuf>,
    seen: Seen,
    docs: CrdtDocs,
    peer_id: String,
    config: Arc<RwLock<SyncConfig>>,
    catalog: Catalog,
    engine: Option<Arc<SyncEngine>>,
) -> Result<()> {
    // 1) TLS handshake
    let mut tls = if is_server {
        net::accept_tls(tcp, server_cfg)?
    } else {
        net::connect_tls(tcp, client_cfg)?
    };
    println!(
        "[minisync] TLS handshake complete ({})",
        if is_server { "server" } else { "client" }
    );

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

    // Notify GUI of new peer
    if let Some(eng) = &engine {
        eng.notify_gui(EngineEvent::PeerConnected {
            remote_id: remote_id.clone(),
        });
    }

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
        &config,
        &catalog,
        engine.as_deref(),
    );

    // 10) 정리: registry에서 제거 → Sender drop → writer thread 종료
    registry.remove(conn_id);
    if let Some(eng) = &engine {
        eng.notify_gui(EngineEvent::PeerDisconnected {
            remote_id: remote_id.clone(),
        });
    }
    drop(peer_conn);
    let _ = writer_handle.join();
    println!(
        "[minisync] peer {remote_id} (conn_id={conn_id}) disconnected, peers={}",
        registry.count()
    );
    result
}

/// Manual buffered reader: lock → read (with timeout) → unlock → parse.
pub fn reader_loop_buffered(
    stream: &Arc<Mutex<TlsStream>>,
    peer_conn: &Arc<PeerConn>,
    root: &Path,
    seen: &Seen,
    docs: &CrdtDocs,
    peer_id: &str,
    remote_id: &str,
    config: &Arc<RwLock<SyncConfig>>,
    catalog: &Catalog,
    engine: Option<&SyncEngine>,
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
            let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
            if buf.len() < 4 + len {
                break; // incomplete message, wait for more data
            }
            let msg: Message = bincode::deserialize(&buf[4..4 + len])?;
            buf.drain(..4 + len);
            handle_message(msg, peer_conn, root, seen, docs, peer_id, remote_id, config, catalog, engine)?;
        }
    }
}
