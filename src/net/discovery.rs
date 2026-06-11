//! UDP subnet-broadcast 기반 자동 피어 발견 (LAN auto-discovery).
//!
//! 같은 LAN의 노드들이 수동 설정 없이 서로를 찾아 자동 연결한다.
//! 두 개의 루프로 동작:
//!   - `beacon_broadcast_loop`: 주기적으로 자신의 존재를 브로드캐스트.
//!   - `beacon_listen_loop`: 비콘 수신 시, 미연결 피어에 자동 연결.
//!
//! 왜 subnet-directed broadcast(예: 10.47.255.255)인가:
//!   - macOS는 255.255.255.255 limited broadcast에 "No route to host"를 낸다.
//!   - 멀티캐스트는 일부 네트워크(VPN/방화벽)에서 egress가 막힌다.
//!   - 인터페이스별 서브넷 브로드캐스트는 일반 유니캐스트 라우팅을 타므로
//!     같은 호스트(loopback 포함)·다른 머신 모두에 안정적으로 도달한다.
//!
//! 패킷 형식: `MAGIC(4) + bincode(Beacon)`. MAGIC으로 무관한 UDP 트래픽 필터링.
//! 수동 피어 연결(peer_addrs)과 병행 동작하며, 중복 연결은 PeerRegistry가 차단.

use crate::catalog::Catalog;
use crate::config::SyncConfig;
use crate::engine::{CrdtDocs, Seen, SyncEngine};
use crate::net::peers::PeerRegistry;
use rustls::{ClientConfig, ServerConfig};
use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// 패킷 식별용 매직 프리픽스. 무관한 UDP 패킷을 거른다.
const MAGIC: &[u8; 4] = b"MSYN";
/// 모든 노드가 공유하는 고정 UDP 발견 포트.
const BEACON_PORT: u16 = 19531;
/// 비콘 브로드캐스트 간격.
const BEACON_INTERVAL: Duration = Duration::from_secs(3);

/// 브로드캐스트로 자신을 알리는 비콘 페이로드.
#[derive(Debug, Serialize, Deserialize)]
struct Beacon {
    peer_id: String,
    node_name: String,
    /// 이 노드의 TCP listen 포트 (수신 측이 여기로 연결).
    listen_port: u16,
}

/// 주기적으로 LAN의 각 인터페이스 브로드캐스트 주소로 비콘을 보낸다.
/// (전용 스레드에서 무한 루프)
pub fn beacon_broadcast_loop(peer_id: String, node_name: String, listen_port: u16) {
    let socket = match build_sender_socket() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[discovery] broadcast socket setup failed: {e}");
            return;
        }
    };
    println!("[discovery] broadcasting beacons on udp/{BEACON_PORT}");

    loop {
        let beacon = Beacon {
            peer_id: peer_id.clone(),
            node_name: node_name.clone(),
            listen_port,
        };
        match bincode::serialize(&beacon) {
            Ok(payload) => {
                let mut buf = Vec::with_capacity(MAGIC.len() + payload.len());
                buf.extend_from_slice(MAGIC);
                buf.extend_from_slice(&payload);
                // 인터페이스 구성은 바뀔 수 있으니(네트워크 전환) 매 주기 재조회.
                for dest in broadcast_targets() {
                    if let Err(e) = socket.send_to(&buf, SocketAddr::from((dest, BEACON_PORT))) {
                        eprintln!("[discovery] beacon send to {dest} failed: {e}");
                    }
                }
            }
            Err(e) => eprintln!("[discovery] beacon serialize failed: {e}"),
        }
        std::thread::sleep(BEACON_INTERVAL);
    }
}

/// 비콘을 수신하고, 새 피어 발견 시 자동 연결 스레드를 띄운다.
/// (전용 스레드에서 무한 루프) — `connect_fn`은 발견된 주소로 연결을 수행.
#[allow(clippy::too_many_arguments)]
pub fn beacon_listen_loop(
    my_peer_id: String,
    registry: Arc<PeerRegistry>,
    root: Arc<PathBuf>,
    seen: Seen,
    docs: CrdtDocs,
    node_name: String,
    server_cfg: Arc<ServerConfig>,
    client_cfg: Arc<ClientConfig>,
    config: Arc<RwLock<SyncConfig>>,
    catalog: Catalog,
    engine: Option<Arc<SyncEngine>>,
    connect_fn: ConnectFn,
) {
    let socket = match build_listener_socket() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[discovery] listen setup on udp/{BEACON_PORT} failed: {e} (auto-discovery disabled)");
            return;
        }
    };
    println!("[discovery] listening for beacons on udp/{BEACON_PORT}");

    let mut buf = [0u8; 1024];
    loop {
        let (n, src) = match socket.recv_from(&mut buf) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[discovery] recv error: {e}");
                continue;
            }
        };
        // MAGIC 검사 후 페이로드 역직렬화.
        if n < MAGIC.len() || &buf[..MAGIC.len()] != MAGIC {
            continue;
        }
        let beacon: Beacon = match bincode::deserialize(&buf[MAGIC.len()..n]) {
            Ok(b) => b,
            Err(_) => continue,
        };

        // 자기 자신의 비콘은 무시.
        if beacon.peer_id == my_peer_id {
            continue;
        }
        // 이미 연결된 피어는 무시.
        if registry.has_peer(&beacon.peer_id) {
            continue;
        }

        let addr = format!("{}:{}", src.ip(), beacon.listen_port);
        println!(
            "[discovery] found peer {} ({}) at {addr}, connecting...",
            beacon.peer_id, beacon.node_name
        );

        let (reg, r, s, d, pid, nn, scfg, ccfg, cfg, cat, eng) = (
            Arc::clone(&registry),
            Arc::clone(&root),
            Arc::clone(&seen),
            Arc::clone(&docs),
            my_peer_id.clone(),
            node_name.clone(),
            Arc::clone(&server_cfg),
            Arc::clone(&client_cfg),
            Arc::clone(&config),
            catalog.clone(),
            engine.clone(),
        );
        let f = connect_fn;
        std::thread::spawn(move || {
            f(&addr, reg, r, s, d, pid, nn, scfg, ccfg, cfg, cat, eng);
        });
    }
}

/// 브로드캐스트 송신용 소켓 생성 (SO_BROADCAST 설정).
fn build_sender_socket() -> std::io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_broadcast(true)?;
    socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)).into())?;
    Ok(socket.into())
}

/// 비콘을 보낼 브로드캐스트 대상 주소 목록.
/// up 상태인 각 IPv4 인터페이스의 서브넷 브로드캐스트 주소를 모은다.
/// loopback의 브로드캐스트(127.255.255.255)도 포함해 같은 호스트 인스턴스끼리 발견.
/// 하나도 없으면 limited broadcast로 폴백.
fn broadcast_targets() -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            if let if_addrs::IfAddr::V4(v4) = iface.addr {
                if let Some(bcast) = v4.broadcast {
                    if !out.contains(&bcast) {
                        out.push(bcast);
                    }
                }
            }
        }
    }
    if out.is_empty() {
        out.push(Ipv4Addr::BROADCAST);
    }
    out
}

/// 브로드캐스트 수신용 리스너 소켓 생성.
/// SO_REUSEADDR + SO_REUSEPORT로 같은 호스트의 다중 인스턴스 바인딩을 허용한다.
fn build_listener_socket() -> std::io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    socket.set_broadcast(true)?;
    socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, BEACON_PORT)).into())?;
    Ok(socket.into())
}

/// `connect_with_retry`와 동일한 시그니처의 연결 함수 포인터.
/// main.rs의 `connect_with_retry`를 그대로 넘겨 재사용한다.
pub type ConnectFn = fn(
    &str,
    Arc<PeerRegistry>,
    Arc<PathBuf>,
    Seen,
    CrdtDocs,
    String,
    String,
    Arc<ServerConfig>,
    Arc<ClientConfig>,
    Arc<RwLock<SyncConfig>>,
    Catalog,
    Option<Arc<SyncEngine>>,
);
