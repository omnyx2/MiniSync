//! The wire protocol: what two peers say to each other, and how messages are
//! framed on the TCP stream.
//!
//! Framing rule: every message is `[u32 big-endian length][bincode bytes]`.
//! That length prefix is what lets the receiver know where one message ends and
//! the next begins on a byte stream that has no inherent boundaries.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

use crate::catalog::NodeInfo;
use crate::engine::history::HistoryEntry;
use crate::index::FileEntry;

/// Everything one peer can say to the other.
#[derive(Debug, Serialize, Deserialize)]
pub enum Message {
    /// First message: exchange peer identity and node name.
    Hello { peer_id: String, node_name: String },
    /// "Here is everything I currently have." Sent right after connecting.
    Index(Vec<FileEntry>),
    /// "Please send me this file (I'm missing it or mine is older)."
    Request(String),
    /// A file's metadata plus its full contents (whole-file transfer for now).
    File { entry: FileEntry, contents: Vec<u8> },
    /// "I deleted this file (relative path) — you should too."
    Delete(String),
    /// Full Automerge document for initial CRDT-lane sync.
    CrdtSync { path: String, data: Vec<u8> },
    /// Incremental Automerge changes for an ongoing CRDT-lane edit.
    CrdtChanges { path: String, changes: Vec<u8> },
    // ── v2: Reference mode ──
    /// Reference file metadata (no contents). Sent for files in Reference mode.
    RefIndex(Vec<RefEntry>),
    /// Request to download a reference-only file from the owner peer.
    DownloadRequest(String),
    /// "I now hold (present=true) / no longer hold (present=false) this file."
    /// Lets peers keep the holder set live as copies are downloaded or evicted.
    HolderUpdate {
        path: String,
        node: NodeInfo,
        present: bool,
    },
    /// A single change-history entry, broadcast by the node that made the change.
    HistoryAppend(HistoryEntry),
    /// A batch of recent history entries, sent on connect so a peer catches up.
    HistorySync(Vec<HistoryEntry>),
    /// Liveness heartbeat. Receipt alone proves the peer is alive; no reply needed.
    Ping,
}

/// Maximum accepted frame size (the bincode payload after the 4-byte length
/// prefix). A peer announces a `u32` length, so without a cap a single frame
/// could force a ~4 GiB allocation (`vec![0u8; len]`) — a trivial memory-bomb
/// DoS. Because files transfer whole (`Message::File { contents }`), this also
/// acts as the effective max syncable file size; bump it if you sync larger
/// files (chunked transfer would remove the ceiling — see scaling plan).
pub const MAX_MSG_SIZE: usize = 1 << 30; // 1 GiB

/// Metadata for a reference-mode file (no contents transferred).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefEntry {
    pub path: String,
    pub size: u64,
    pub hash: String,
    pub mtime: i64,
    pub owner_id: String,
    pub owner_name: String,
    /// The file's original creator (immutable). `None` for legacy entries.
    #[serde(default)]
    pub origin: Option<NodeInfo>,
}

/// Write one length-prefixed message.
pub fn send_msg<W: Write>(w: &mut W, msg: &Message) -> Result<()> {
    let buf = serialize_msg(msg)?;
    w.write_all(&buf)?;
    w.flush()?;
    Ok(())
}

/// Serialize a message to length-prefixed bytes (for channel-based sending).
pub fn serialize_msg(msg: &Message) -> Result<Vec<u8>> {
    let payload = bincode::serialize(msg)?;
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(&payload);
    Ok(buf)
}

/// Read one length-prefixed message. `Ok(None)` means the peer closed cleanly.
pub fn recv_msg<R: Read>(r: &mut R) -> Result<Option<Message>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_MSG_SIZE {
        anyhow::bail!("frame too large: {len} bytes (max {MAX_MSG_SIZE})");
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(Some(bincode::deserialize(&buf)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn recv_rejects_oversized_frame_without_allocating() {
        // A length prefix claiming MAX_MSG_SIZE + 1, then no body. recv_msg must
        // bail on the length check before it ever tries to allocate the buffer.
        let bogus_len = (MAX_MSG_SIZE as u64 + 1).min(u32::MAX as u64) as u32;
        let header = bogus_len.to_be_bytes();
        let mut cur = Cursor::new(header.to_vec());
        let err = recv_msg(&mut cur).unwrap_err();
        assert!(err.to_string().contains("frame too large"), "got: {err}");
    }

    #[test]
    fn roundtrip_small_message_ok() {
        let msg = Message::Ping;
        let bytes = serialize_msg(&msg).unwrap();
        let mut cur = Cursor::new(bytes);
        let got = recv_msg(&mut cur).unwrap();
        assert!(matches!(got, Some(Message::Ping)));
    }
}
