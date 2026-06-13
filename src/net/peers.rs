//! PeerRegistry: 복수 피어 연결을 관리하는 스레드 안전 레지스트리.
//!
//! Full mesh P2P: 모든 노드가 listen + connect 동시 수행.
//! 변경사항은 originator의 watcher가 모든 직접 피어에 broadcast.
//!
//! TLS 도입으로 writer는 mpsc 채널 기반. 연결당 writer thread가 실제 TLS write 수행.

use crate::protocol::{serialize_msg, Message};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};

/// 연결 슬롯 식별자. 단조 증가하는 u64로 ABA 문제 방지.
pub type ConnId = u64;

/// 하나의 연결된 피어. writer는 채널 기반 (TLS stream은 clone 불가).
#[allow(dead_code)]
pub struct PeerConn {
    pub conn_id: ConnId,
    pub remote_id: String,
    pub remote_name: String,
    pub writer: mpsc::Sender<Vec<u8>>,
    /// 중복 연결 해소 시 set. 펌프 루프가 이를 보고 스스로 종료한다.
    pub evicted: AtomicBool,
}

/// Snapshot of peer info for GUI display.
#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub conn_id: ConnId,
    pub remote_id: String,
    pub remote_name: String,
}

/// 현재 연결된 모든 피어의 중앙 레지스트리.
pub struct PeerRegistry {
    next_id: Mutex<u64>,
    peers: RwLock<HashMap<ConnId, Arc<PeerConn>>>,
}

impl PeerRegistry {
    pub fn new() -> Self {
        PeerRegistry {
            next_id: Mutex::new(1),
            peers: RwLock::new(HashMap::new()),
        }
    }

    /// 새 피어 등록 (atomic dedup).
    ///
    /// 두 노드가 거의 동시에 서로에게 connect 하면 같은 피어쌍에 TCP 연결이 2개
    /// 생긴다(각자 outbound 1 + inbound 1). 어느 하나를 살릴지 양쪽이 *독립적으로*
    /// 고르면 서로 다른 연결을 살려 엇갈리거나(한 방향만 동기화) 둘 다 닫혀 붕괴한다.
    ///
    /// 해결: **결정적 tie-break** — "peer_id가 작은 쪽이 client인 연결"만 살린다.
    /// 두 노드가 동일한 peer_id 쌍·동일한 client/server 토폴로지를 보므로 항상 같은
    /// 물리 연결로 수렴한다. 중복이 없으면(연결 1개) 무조건 등록한다.
    ///
    /// `local_is_client`: 이 연결에서 우리가 connect 측인가(= !is_server).
    /// `my_id`: 우리 peer_id.
    pub fn add_if_new(
        &self,
        remote_id: String,
        remote_name: String,
        local_is_client: bool,
        my_id: &str,
        writer: mpsc::Sender<Vec<u8>>,
    ) -> Option<(ConnId, Arc<PeerConn>)> {
        let mut peers = self.peers.write().unwrap();

        let existing: Vec<ConnId> = peers
            .iter()
            .filter(|(_, p)| p.remote_id == remote_id)
            .map(|(id, _)| *id)
            .collect();

        if !existing.is_empty() {
            // 이 연결이 "선호" 토폴로지인가: smaller peer_id == client.
            let incoming_preferred = local_is_client == (my_id < remote_id.as_str());
            if incoming_preferred {
                // 기존(비선호) 연결을 축출하고 이 연결을 등록.
                for id in existing {
                    if let Some(old) = peers.remove(&id) {
                        old.evicted.store(true, Ordering::SeqCst);
                    }
                }
            } else {
                // 기존(선호) 연결 유지, 이 연결은 거부.
                return None;
            }
        }

        let conn_id = {
            let mut id = self.next_id.lock().unwrap();
            let c = *id;
            *id += 1;
            c
        };
        let peer = Arc::new(PeerConn {
            conn_id,
            remote_id,
            remote_name,
            writer,
            evicted: AtomicBool::new(false),
        });
        peers.insert(conn_id, Arc::clone(&peer));
        Some((conn_id, peer))
    }

    /// 피어 제거 (연결 해제 시). 멱등.
    pub fn remove(&self, conn_id: ConnId) {
        self.peers.write().unwrap().remove(&conn_id);
    }

    /// 모든 피어에 메시지 전송. 개별 실패는 로그만 남기고 무시.
    pub fn broadcast(&self, msg: &Message) {
        let peers = self.peers.read().unwrap();
        for (id, peer) in peers.iter() {
            if let Err(e) = send_to_peer_inner(peer, msg) {
                eprintln!("[broadcast] conn_id={id}: {e}");
            }
        }
    }

    pub fn count(&self) -> usize {
        self.peers.read().unwrap().len()
    }

    /// 주어진 remote_id의 피어가 이미 연결되어 있는지 확인 (auto-discovery용).
    pub fn has_peer(&self, remote_id: &str) -> bool {
        self.peers
            .read()
            .unwrap()
            .values()
            .any(|p| p.remote_id == remote_id)
    }

    /// Get a snapshot of all connected peers (for GUI).
    pub fn peer_list(&self) -> Vec<PeerInfo> {
        let peers = self.peers.read().unwrap();
        peers
            .values()
            .map(|p| PeerInfo {
                conn_id: p.conn_id,
                remote_id: p.remote_id.clone(),
                remote_name: p.remote_name.clone(),
            })
            .collect()
    }

    /// Find a peer by remote_id and send a message (unicast).
    pub fn send_to_remote(&self, remote_id: &str, msg: &Message) -> Result<()> {
        let peers = self.peers.read().unwrap();
        for peer in peers.values() {
            if peer.remote_id == remote_id {
                return send_to_peer_inner(peer, msg);
            }
        }
        Err(anyhow::anyhow!("peer {remote_id} not found"))
    }
}

/// 단일 피어에 메시지 전송 (유니캐스트). 채널을 통해 writer thread에 전달.
pub fn send_to_peer(peer: &PeerConn, msg: &Message) -> Result<()> {
    send_to_peer_inner(peer, msg)
}

fn send_to_peer_inner(peer: &PeerConn, msg: &Message) -> Result<()> {
    let bytes = serialize_msg(msg)?;
    peer.writer
        .send(bytes)
        .map_err(|_| anyhow::anyhow!("peer channel closed"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_writer() -> mpsc::Sender<Vec<u8>> {
        mpsc::channel().0
    }

    /// 동시 connect로 생긴 두 연결이 등록 순서와 무관하게 항상 동일한
    /// "선호" 연결(작은 peer_id가 client)로 수렴해야 한다.
    #[test]
    fn dedup_converges_on_preferred_connection() {
        // 우리 id="aaaa", 상대="bbbb". 선호 = 작은 id("aaaa")가 client →
        // 이 노드에서는 우리가 client인 연결이 선호.

        // 순서 1: 비선호(server)가 먼저 등록 → 선호(client)가 축출하고 등록.
        let reg = PeerRegistry::new();
        let (_id, server_peer) = reg
            .add_if_new("bbbb".into(), "B".into(), false, "aaaa", dummy_writer())
            .expect("first conn registers");
        assert!(reg
            .add_if_new("bbbb".into(), "B".into(), true, "aaaa", dummy_writer())
            .is_some());
        assert!(
            server_peer.evicted.load(Ordering::SeqCst),
            "non-preferred conn must be evicted"
        );
        assert_eq!(reg.count(), 1);

        // 순서 2: 선호(client)가 먼저 → 비선호(server)는 거부.
        let reg2 = PeerRegistry::new();
        assert!(reg2
            .add_if_new("bbbb".into(), "B".into(), true, "aaaa", dummy_writer())
            .is_some());
        assert!(
            reg2.add_if_new("bbbb".into(), "B".into(), false, "aaaa", dummy_writer())
                .is_none(),
            "non-preferred conn must be rejected when preferred already present"
        );
        assert_eq!(reg2.count(), 1);
    }

    /// 중복이 없으면(연결 1개) 토폴로지/ id 순서와 무관하게 무조건 등록해야 한다.
    /// (manual-peer 단방향 등 정상 단일 연결이 깨지지 않도록 하는 회귀 방지.)
    #[test]
    fn lone_connection_always_registers() {
        let reg = PeerRegistry::new();
        // 우리가 server이고 우리 id가 더 큼 → "비선호" 토폴로지이지만 유일 연결.
        assert!(
            reg.add_if_new("aaaa".into(), "A".into(), false, "zzzz", dummy_writer())
                .is_some(),
            "lone connection must register even with non-preferred topology"
        );
        assert_eq!(reg.count(), 1);
    }
}
