//! The heart of the tool: keep N peers' folders in sync over a full mesh P2P.
//!
//! SyncEngine holds all shared state. Sub-modules handle specific concerns:
//!   - session: per-peer TLS session lifecycle
//!   - handlers: message dispatch and individual handlers
//!   - watch: local file change monitoring loop
//!   - transfer: file I/O helpers (apply, hash, conflict)

pub mod handlers;
pub mod session;
pub mod transfer;
pub mod watch;

use automerge::AutoCommit;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, RwLock};

use crate::catalog::Catalog;
use crate::config::SyncConfig;
use crate::net::peers::PeerRegistry;

/// Map of relative-path → last-sent SHA-256 hash. Prevents re-sending unchanged files.
pub type Seen = Arc<Mutex<HashMap<String, String>>>;

/// Map of relative-path → in-memory Automerge CRDT document.
pub type CrdtDocs = Arc<Mutex<HashMap<String, AutoCommit>>>;

/// Marker hash for deleted files.
pub const DELETED_HASH: &str = "";

/// Events from the engine to the GUI.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    /// The catalog has been updated (file added, removed, or changed).
    CatalogUpdated,
    /// A new peer connected.
    PeerConnected { remote_id: String },
    /// A peer disconnected.
    PeerDisconnected { remote_id: String },
    /// An error occurred.
    Error(String),
}

/// Commands from the GUI to the engine.
#[derive(Debug, Clone)]
pub enum GuiCommand {
    /// Request to download a reference-mode file.
    Download(String),
    /// Update the sync configuration.
    UpdateConfig(SyncConfig),
    /// Rescan the sync folder.
    Rescan,
}

/// Shared state for the sync engine, passed to all subsystems.
pub struct SyncEngine {
    pub root: Arc<PathBuf>,
    pub peer_id: String,
    pub node_name: String,
    pub registry: Arc<PeerRegistry>,
    pub seen: Seen,
    pub docs: CrdtDocs,
    pub config: Arc<RwLock<SyncConfig>>,
    pub catalog: Catalog,
    pub gui_tx: Option<Sender<EngineEvent>>,
    pub gui_rx: Option<Mutex<Receiver<GuiCommand>>>,
}

impl SyncEngine {
    /// Notify the GUI (if connected) of an event.
    pub fn notify_gui(&self, event: EngineEvent) {
        if let Some(tx) = &self.gui_tx {
            let _ = tx.send(event);
        }
    }
}
