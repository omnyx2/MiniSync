//! Per-peer session lifecycle: TLS handshake → Hello → Index → reader loop.

use anyhow::{bail, Result};
use rustls::{ClientConfig, ServerConfig};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use super::handlers::handle_message;
use super::{CrdtDocs, EngineEvent, Seen, SyncEngine};
use crate::catalog::Catalog;
use crate::config::SyncConfig;
use crate::index::{build_index, FileEntry};
use crate::net::peers::{send_to_peer, PeerConn, PeerRegistry};
use crate::net::{self, TlsStream};
use crate::protocol::{recv_msg, send_msg, serialize_msg, Message};
use std::time::Instant;

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
    node_name: String,
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

    // 2) Hello 교환 — exchange peer_id + node_name
    send_msg(
        &mut tls,
        &Message::Hello {
            peer_id: peer_id.clone(),
            node_name: node_name.clone(),
        },
    )?;
    let (remote_id, remote_name) = match recv_msg(&mut tls)? {
        Some(Message::Hello { peer_id, node_name }) => (peer_id, node_name),
        _ => bail!("expected Hello from peer"),
    };
    println!("[minisync] remote peer: {remote_id} ({remote_name})");

    // 3) Channel for outbound messages (broadcast/unicast feed this; pump drains it)
    let (tx, rx) = mpsc::channel::<Vec<u8>>();

    // 4) Atomic 등록 + 결정적 중복연결 해소 (peers::add_if_new 참고).
    //    동시 connect로 생긴 두 연결 중 "작은 peer_id가 client인 쪽"만 양 노드가
    //    일관되게 살린다. 비선호 연결은 거부(여기서 close)된다.
    let (conn_id, peer_conn) =
        match registry.add_if_new(remote_id.clone(), remote_name.clone(), !is_server, &peer_id, tx) {
            Some(pair) => pair,
            None => {
                println!("[minisync] duplicate connection to {remote_id}, dropping (non-preferred)");
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

    // 5) Index 전송 (큐에 적재; 펌프가 내보냄)
    let entries: Vec<FileEntry> = build_index(&root)?.into_values().collect();
    send_to_peer(&peer_conn, &Message::Index(entries))?;

    // 5b) 최근 변경 이력 전송 — 오프라인이었던 피어가 공유 이력을 따라잡게.
    if let Some(eng) = &engine {
        let hist = eng.history.recent(200);
        if !hist.is_empty() {
            send_to_peer(&peer_conn, &Message::HistorySync(hist))?;
        }
    }

    // 6) 단일 스레드 논블로킹 full-duplex 펌프.
    //    reader/writer가 하나의 뮤텍스를 공유하던 구조를 없애 교착을 제거한다:
    //    매 루프마다 (보낼 것 보내고) + (받을 것 받으므로) 한 방향의 백프레셔가
    //    다른 방향을 굶기지 않는다.
    tls.set_nonblocking(true)?;
    let result = pump_loop(
        &mut tls,
        &rx,
        &peer_conn,
        &root,
        &seen,
        &docs,
        &peer_id,
        &node_name,
        &remote_id,
        &remote_name,
        &config,
        &catalog,
        engine.as_deref(),
    );

    // 7) 정리: registry에서 제거
    registry.remove(conn_id);
    if let Some(eng) = &engine {
        eng.notify_gui(EngineEvent::PeerDisconnected {
            remote_id: remote_id.clone(),
        });
    }
    drop(peer_conn);
    println!(
        "[minisync] peer {remote_id} (conn_id={conn_id}) disconnected, peers={}",
        registry.count()
    );
    result
}

/// 단일 스레드 full-duplex 펌프 (논블로킹 소켓).
///
/// 매 반복: (a) 아웃바운드 채널 → rustls 송신버퍼, (b) rustls → 소켓 flush,
/// (c) 소켓 → rustls 읽기, (d) 평문 디코드, (e) 프레임 파싱·디스패치.
/// 어느 단계도 소켓에서 무한정 블로킹하지 않으므로, 큰 파일 동시 전송 시에도
/// 송신 백프레셔가 수신을 막지 않는다.
#[allow(clippy::too_many_arguments)]
fn pump_loop(
    tls: &mut TlsStream,
    rx: &mpsc::Receiver<Vec<u8>>,
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
    let mut inbuf = Vec::new();
    let mut tmp = [0u8; 16384];

    // Liveness heartbeat: ping every PING_INTERVAL; if nothing is received for
    // PEER_TIMEOUT the peer is presumed dead and we drop the connection (so peer
    // lists / availability reflect reality promptly instead of lingering on a
    // half-open TCP socket). Both sides ping, so an idle-but-alive link stays up.
    const PING_INTERVAL: Duration = Duration::from_secs(4);
    const PEER_TIMEOUT: Duration = Duration::from_secs(12);
    let ping_bytes = serialize_msg(&Message::Ping)?;
    let mut last_recv = Instant::now();
    let mut last_ping = Instant::now();

    loop {
        // (0) 중복연결 해소로 축출되었으면 즉시 종료 (소켓 닫힘).
        if peer_conn.evicted.load(std::sync::atomic::Ordering::SeqCst) {
            println!("[minisync] conn to {remote_id} evicted (duplicate resolved)");
            return Ok(());
        }

        let mut did_work = false;

        // (0b) Heartbeat: drop a silent/dead peer; otherwise ping periodically.
        let now = Instant::now();
        if now.duration_since(last_recv) > PEER_TIMEOUT {
            println!("[sync] peer {remote_id} timed out (no data for >{}s)", PEER_TIMEOUT.as_secs());
            return Ok(());
        }
        if now.duration_since(last_ping) >= PING_INTERVAL {
            tls.conn.writer().write_all(&ping_bytes)?;
            last_ping = now;
            did_work = true;
        }

        // (a) 아웃바운드 메시지를 rustls 송신 버퍼에 적재 (소켓 I/O 없음)
        loop {
            match rx.try_recv() {
                Ok(bytes) => {
                    tls.conn.writer().write_all(&bytes)?;
                    did_work = true;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return Ok(()), // 모든 sender drop
            }
        }

        // (b) 송신 버퍼를 소켓으로 flush (논블로킹; WouldBlock이면 다음 기회에)
        while tls.conn.wants_write() {
            match tls.conn.write_tls(&mut tls.sock) {
                Ok(0) => break,
                Ok(_) => did_work = true,
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.into()),
            }
        }

        // (c) 소켓에서 TLS 레코드 읽기 (논블로킹)
        match tls.conn.read_tls(&mut tls.sock) {
            Ok(0) => {
                println!("[sync] peer {remote_id} disconnected (EOF)");
                return Ok(());
            }
            Ok(_) => {
                tls.conn.process_new_packets()?;
                did_work = true;
                last_recv = Instant::now(); // peer is alive
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }

        // (d) 복호화된 평문을 inbuf로 흡수
        loop {
            match tls.conn.reader().read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    inbuf.extend_from_slice(&tmp[..n]);
                    did_work = true;
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.into()),
            }
        }

        // (e) 완성된 프레임 파싱·디스패치
        while inbuf.len() >= 4 {
            let len = u32::from_be_bytes([inbuf[0], inbuf[1], inbuf[2], inbuf[3]]) as usize;
            if inbuf.len() < 4 + len {
                break; // 불완전 — 더 받을 때까지 대기
            }
            let msg: Message = bincode::deserialize(&inbuf[4..4 + len])?;
            inbuf.drain(..4 + len);
            handle_message(
                msg, peer_conn, root, seen, docs, peer_id, node_name, remote_id, remote_name,
                config, catalog, engine,
            )?;
        }

        // (f) 할 일이 없으면 잠깐 쉰다. 보낼 게 막혀있으면 더 짧게.
        if !did_work {
            let nap = if tls.conn.wants_write() { 1 } else { 10 };
            std::thread::sleep(Duration::from_millis(nap));
        }
    }
}
