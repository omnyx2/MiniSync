//! GuiBridge: communication channel between the GUI and the sync engine.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, RwLock};

use crate::catalog::Catalog;
use crate::config::SyncConfig;
use crate::engine::{EngineEvent, GuiCommand, SyncEngine};
use crate::net::peers::PeerRegistry;

/// Bridge between the GUI and the engine.
/// The GUI reads events and shared state; sends commands to the engine.
pub struct GuiBridge {
    /// Receive engine events (catalog updates, peer changes, errors).
    pub events_rx: Receiver<EngineEvent>,
    /// Send commands to the engine (download, config update, rescan).
    pub commands_tx: Sender<GuiCommand>,
    /// Read-only access to the unified file catalog.
    pub catalog: Catalog,
    /// Read-only access to the peer registry.
    pub registry: Arc<PeerRegistry>,
    /// Shared config (read for display, write via GuiCommand).
    pub config: Arc<RwLock<SyncConfig>>,
    /// Sync root folder — needed for drag-and-drop file import.
    pub root: Arc<PathBuf>,
    /// This node's human-readable name.
    pub node_name: String,
    /// The sync engine — used to install the repaint hook so engine events
    /// wake the GUI immediately instead of waiting for the idle timer.
    pub engine: Arc<SyncEngine>,
}
