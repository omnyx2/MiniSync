//! PeerRegistry: 복수 피어 연결을 관리하는 스레드 안전 레지스트리.
//!
//! Full mesh P2P: 모든 노드가 listen + connect 동시 수행.
//! 변경사항은 originator의 watcher가 모든 직접 피어에 broadcast.
//!
//! TLS 도입으로 writer는 mpsc 채널 기반. 연결당 writer thread가 실제 TLS write 수행.

use crate::protocol::{serialize_msg, Message};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Arc, RwLock, Mutex};

/// 연결 슬롯 식별자. 단조 증가하는 u64로 ABA 문제 방지.
pub type ConnId = u64;

/// 하나의 연결된 피어. writer는 채널 기반 (TLS stream은 clone 불가).
#[allow(dead_code)]
pub struct PeerConn {
    pub conn_id: ConnId,
    pub remote_id: String,
    pub writer: mpsc::Sender<Vec<u8>>,
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

    /// 새 피어 등록 (atomic dup check). 이미 같은 remote_id가 있으면 None.
    pub fn add_if_new(
        &self,
        remote_id: String,
        writer: mpsc::Sender<Vec<u8>>,
    ) -> Option<(ConnId, Arc<PeerConn>)> {
        let mut peers = self.peers.write().unwrap();
        if peers.values().any(|p| p.remote_id == remote_id) {
            return None;
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
            writer,
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
