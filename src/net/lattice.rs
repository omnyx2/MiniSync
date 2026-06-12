//! Lattice 오버레이 기반 피어 발견.
//!
//! lattice(메시 VPN) 데몬의 로컬 IPC(`/tmp/lattice.sock`)에 `health_check`를
//! 질의해, 현재 메시망에서 **연결된(connected)** 노드들의 가상 IP(VIP)를 받아
//! 그 VIP로 minisync 피어 연결을 건다. LAN UDP 발견과 동일하게 `ConnectFn`으로
//! `connect_with_retry`를 재사용한다.
//!
//! 와이어 포맷(데몬 소스 기준 확정):
//!   요청  : `{"cmd":"health_check"}\n`  (한 줄 쓰고 한 줄 읽기)
//!   응답  : `{"ok":"health","data":[{"virtual_ip":"100.x.x.x","fingerprint":"..","status":".."}]}`
//!   거부/오류: `{"ok":"error","data":{"message":"..."}}`
//!
//! status: `self`(자기), `connected`(즉시 dial 가능), `connecting`/`known`/`lost`
//! (아직 터널 없음 → dial 금지). minisync는 **connected만** dial 한다.
//!
//! 주의:
//!   - 호출자 프로세스 이름이 정확히 데몬 allow-list(기본 `minisync`)와 같아야
//!     응답한다. 바이너리 이름을 `minisync`로 둘 것(심링크 불가).
//!   - VIP는 노드 정적 키 해시라 재시작에도 고정. 그래서 작은 VIP가 dial하고 큰
//!     VIP가 listen하는 tie-break로 양쪽 동시 dial(중복 연결)을 막는다.

use crate::catalog::Catalog;
use crate::config::SyncConfig;
use crate::engine::{CrdtDocs, Seen, SyncEngine};
use crate::net::discovery::ConnectFn;
use crate::net::peers::PeerRegistry;
use rustls::{ClientConfig, ServerConfig};
use serde::Deserialize;
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::net::Ipv4Addr;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

/// 기본 lattice IPC 소켓 경로.
pub const DEFAULT_SOCKET: &str = "/tmp/lattice.sock";
/// health_check 폴링 간격.
const POLL_INTERVAL: Duration = Duration::from_secs(8);
/// 소켓 읽기 타임아웃.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// health_check 응답의 노드 한 개.
#[derive(Debug, Deserialize)]
struct HealthEntry {
    virtual_ip: String,
    #[allow(dead_code)]
    fingerprint: String,
    status: String,
}

/// `{"ok": "...", "data": ...}` 의 느슨한 표현(variant 태깅 직접 처리).
#[derive(Debug, Deserialize)]
struct RawResponse {
    ok: String,
    data: serde_json::Value,
}

/// 데몬에 health_check 1회 질의. 성공 시 엔트리 목록 반환.
fn query_health(socket_path: &str) -> Result<Vec<HealthEntry>, String> {
    let stream = UnixStream::connect(socket_path).map_err(|e| format!("connect {socket_path}: {e}"))?;
    stream.set_read_timeout(Some(IO_TIMEOUT)).ok();
    stream.set_write_timeout(Some(IO_TIMEOUT)).ok();

    let mut writer = stream.try_clone().map_err(|e| format!("clone: {e}"))?;
    writer
        .write_all(b"{\"cmd\":\"health_check\"}\n")
        .map_err(|e| format!("write: {e}"))?;
    writer.flush().ok();

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| format!("read: {e}"))?;

    let raw: RawResponse =
        serde_json::from_str(line.trim()).map_err(|e| format!("parse '{}': {e}", line.trim()))?;
    match raw.ok.as_str() {
        "health" => serde_json::from_value(raw.data).map_err(|e| format!("health payload: {e}")),
        "error" => Err(raw
            .data
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error")
            .to_string()),
        other => Err(format!("unexpected response variant '{other}'")),
    }
}

/// 데몬을 주기적으로 폴링하며, 새로 connected된 피어의 VIP로 연결을 건다.
/// (전용 스레드에서 무한 루프)
#[allow(clippy::too_many_arguments)]
pub fn lattice_discovery_loop(
    socket_path: String,
    dial_port: u16,
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
    println!(
        "[lattice] discovery enabled via {socket_path} — dialing connected peers on :{dial_port}"
    );
    // 현재 dial 중(또는 세션 유지 중)인 주소. 세션 종료 시 제거되어 재연결 허용.
    let dialing: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let mut last_ok = true; // 에러 로그를 상태 전이 시에만 찍기 위함.

    loop {
        match query_health(&socket_path) {
            Ok(entries) => {
                if !last_ok {
                    println!("[lattice] health_check OK ({} nodes)", entries.len());
                    last_ok = true;
                }
                // 내 VIP(self) 추출 — tie-break 기준.
                let self_vip = entries
                    .iter()
                    .find(|e| e.status == "self")
                    .and_then(|e| e.virtual_ip.parse::<Ipv4Addr>().ok());
                let self_vip = match self_vip {
                    Some(v) => v,
                    // 아직 VIP 미할당(데몬 부팅 중) — 다음 주기에 재시도.
                    None => {
                        std::thread::sleep(POLL_INTERVAL);
                        continue;
                    }
                };

                for e in &entries {
                    if e.status != "connected" {
                        continue; // self/connecting/known/lost → dial 안 함
                    }
                    let peer_vip = match e.virtual_ip.parse::<Ipv4Addr>() {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    // tie-break: 작은 VIP만 dial하고 큰 쪽은 inbound를 기다린다.
                    // (양쪽이 서로 dial해 중복 세션이 생기는 것을 방지)
                    if u32::from(self_vip) >= u32::from(peer_vip) {
                        continue;
                    }

                    let addr = format!("{}:{}", e.virtual_ip, dial_port);
                    {
                        let mut d = dialing.lock().unwrap();
                        if !d.insert(addr.clone()) {
                            continue; // 이미 dial 중
                        }
                    }
                    println!("[lattice] peer {} connected → dialing {addr}", e.fingerprint);

                    let (reg, r, s, dd, pid, nn, scfg, ccfg, cfg, cat, eng) = (
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
                    let dialing_ref = Arc::clone(&dialing);
                    let addr_owned = addr.clone();
                    std::thread::spawn(move || {
                        // 연결 유지 동안 블록되고, 세션 종료/포기 시 반환된다.
                        f(&addr_owned, reg, r, s, dd, pid, nn, scfg, ccfg, cfg, cat, eng);
                        // 제거 → 피어가 여전히 connected면 다음 폴링에서 재연결.
                        dialing_ref.lock().unwrap().remove(&addr_owned);
                    });
                }
            }
            Err(e) => {
                if last_ok {
                    eprintln!("[lattice] health_check unavailable: {e} (will keep retrying)");
                    last_ok = false;
                }
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_health_response() {
        let json = r#"{"ok":"health","data":[
            {"virtual_ip":"100.106.172.240","fingerprint":"cb2aacf0","status":"self"},
            {"virtual_ip":"100.121.37.116","fingerprint":"c2b92574","status":"connected"},
            {"virtual_ip":"100.124.227.94","fingerprint":"7dbce35e","status":"known"}
        ]}"#;
        let raw: RawResponse = serde_json::from_str(json).unwrap();
        assert_eq!(raw.ok, "health");
        let entries: Vec<HealthEntry> = serde_json::from_value(raw.data).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1].virtual_ip, "100.121.37.116");
        assert_eq!(entries[1].status, "connected");
    }

    #[test]
    fn parses_error_response() {
        let json = r#"{"ok":"error","data":{"message":"health check denied for process \"python3\""}}"#;
        let raw: RawResponse = serde_json::from_str(json).unwrap();
        assert_eq!(raw.ok, "error");
        assert!(raw.data.get("message").unwrap().as_str().unwrap().contains("denied"));
    }

    #[test]
    fn tie_break_only_smaller_vip_dials() {
        // self=100.121.37.116, peer=100.124.227.94 → self < peer → dial.
        let me: Ipv4Addr = "100.121.37.116".parse().unwrap();
        let peer: Ipv4Addr = "100.124.227.94".parse().unwrap();
        assert!(u32::from(me) < u32::from(peer), "smaller VIP dials larger");
        // 반대 방향은 dial 안 함.
        assert!(!(u32::from(peer) < u32::from(me)));
    }
}
