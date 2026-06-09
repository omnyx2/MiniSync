//! minisync — a tiny peer-to-peer folder sync (full mesh P2P + TLS).
//!
//! Usage:
//!   minisync <folder> <listen_addr> [peer1_addr] [peer2_addr] ...
//!
//! 모든 노드가 동등: listen + connect 동시 수행.
//! 어떤 노드가 죽어도 나머지끼리 계속 동기화.
//! 통신은 rustls TLS로 암호화 (자체서명 인증서).

mod crdt;
mod index;
mod peers;
mod protocol;
mod routing;
mod sync;
mod tls;
mod watcher;

use anyhow::Result;
use rustls::{ClientConfig, ServerConfig};
use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use peers::PeerRegistry;
use sync::{CrdtDocs, Seen};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("minisync — tiny peer-to-peer folder sync (full mesh + TLS)\n");
        eprintln!("Usage:");
        eprintln!(
            "  {} <folder> <listen_addr> [peer1_addr] [peer2_addr] ...",
            args[0]
        );
        eprintln!();
        eprintln!("Example (3 nodes):");
        eprintln!("  {} ./data 0.0.0.0:9000 10.0.0.2:9001 10.0.0.3:9002", args[0]);
        std::process::exit(1);
    }

    let listen_addr = args[2].clone();
    let peer_addrs: Vec<String> = args[3..]
        .iter()
        .filter(|a| a.as_str() > listen_addr.as_str())
        .cloned()
        .collect();
    std::fs::create_dir_all(&args[1])?;
    let folder: PathBuf = std::fs::canonicalize(&args[1])?;
    let peer_id = generate_peer_id();

    // TLS 설정: 자체서명 인증서 생성
    let (cert, key) = tls::generate_self_signed()?;
    let server_cfg = tls::server_config(cert, key)?;
    let client_cfg = tls::client_config();
    println!("[minisync] TLS configured (self-signed)");

    let root = Arc::new(folder);
    let registry = Arc::new(PeerRegistry::new());
    let seen: Seen = Arc::new(Mutex::new(HashMap::new()));
    let docs: CrdtDocs = Arc::new(Mutex::new(HashMap::new()));

    println!("[minisync] id={peer_id} listening on {listen_addr}, syncing {:?}", &*root);

    // 1) Watcher thread
    {
        let (reg, r, s, d, pid) = (
            Arc::clone(&registry),
            Arc::clone(&root),
            Arc::clone(&seen),
            Arc::clone(&docs),
            peer_id.clone(),
        );
        std::thread::spawn(move || {
            if let Err(e) = sync::watch_loop(reg, r, s, d, pid) {
                eprintln!("[watcher] stopped: {e}");
            }
        });
    }

    // 2) Listener thread (inbound: TLS server role)
    {
        let listener = TcpListener::bind(&listen_addr)?;
        let (reg, r, s, d, pid, scfg, ccfg) = (
            Arc::clone(&registry),
            Arc::clone(&root),
            Arc::clone(&seen),
            Arc::clone(&docs),
            peer_id.clone(),
            Arc::clone(&server_cfg),
            Arc::clone(&client_cfg),
        );
        std::thread::spawn(move || {
            for stream_result in listener.incoming() {
                match stream_result {
                    Ok(stream) => {
                        let addr = stream.peer_addr().ok();
                        println!("[minisync] inbound connection from {addr:?}");
                        let (reg2, r2, s2, d2, pid2, scfg2, ccfg2) = (
                            Arc::clone(&reg),
                            Arc::clone(&r),
                            Arc::clone(&s),
                            Arc::clone(&d),
                            pid.clone(),
                            Arc::clone(&scfg),
                            Arc::clone(&ccfg),
                        );
                        std::thread::spawn(move || {
                            if let Err(e) = sync::run_peer_session(
                                stream, true, scfg2, ccfg2, reg2, r2, s2, d2, pid2,
                            ) {
                                eprintln!("[sync] inbound session ended: {e}");
                            }
                        });
                    }
                    Err(e) => eprintln!("[minisync] accept error: {e}"),
                }
            }
        });
    }

    // 3) Outbound connector threads (TLS client role)
    for addr in peer_addrs {
        let (reg, r, s, d, pid, scfg, ccfg) = (
            Arc::clone(&registry),
            Arc::clone(&root),
            Arc::clone(&seen),
            Arc::clone(&docs),
            peer_id.clone(),
            Arc::clone(&server_cfg),
            Arc::clone(&client_cfg),
        );
        std::thread::spawn(move || {
            connect_with_retry(&addr, reg, r, s, d, pid, scfg, ccfg);
        });
    }

    // 4) Main thread park
    loop {
        std::thread::park();
    }
}

fn connect_with_retry(
    addr: &str,
    registry: Arc<PeerRegistry>,
    root: Arc<PathBuf>,
    seen: Seen,
    docs: CrdtDocs,
    peer_id: String,
    server_cfg: Arc<ServerConfig>,
    client_cfg: Arc<ClientConfig>,
) {
    const MAX_RETRIES: u32 = 5;
    const RETRY_DELAY: Duration = Duration::from_secs(2);

    for attempt in 1..=MAX_RETRIES {
        match TcpStream::connect(addr) {
            Ok(stream) => {
                println!("[minisync] connected to {addr}");
                if let Err(e) = sync::run_peer_session(
                    stream, false, server_cfg, client_cfg, registry, root, seen, docs, peer_id,
                ) {
                    eprintln!("[sync] outbound session to {addr} ended: {e}");
                }
                return;
            }
            Err(e) => {
                if attempt < MAX_RETRIES {
                    println!(
                        "[minisync] connect to {addr} failed ({e}), retry {attempt}/{MAX_RETRIES}..."
                    );
                    std::thread::sleep(RETRY_DELAY);
                } else {
                    println!(
                        "[minisync] giving up on {addr} after {MAX_RETRIES} attempts (peer may connect to us)"
                    );
                }
            }
        }
    }
}

/// 인스턴스별 고유 ID (8자리 hex).
fn generate_peer_id() -> String {
    let mut h = RandomState::new().build_hasher();
    h.write_u32(std::process::id());
    format!("{:08x}", h.finish() as u32)
}
