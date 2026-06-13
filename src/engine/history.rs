//! Change history: a shared, append-only log of *who* changed *what* and *when*.
//! No commit messages, no rollback — an audit trail of sync activity surfaced in
//! the GUI. Persisted as JSON-lines at `.minisync/history.jsonl`.
//!
//! Sharing model: the node where a change originates authors the entry and
//! broadcasts it (`Message::HistoryAppend`); peers `apply_remote` it (deduped by
//! id). On connect a peer also sends its recent log (`Message::HistorySync`) so
//! nodes that were offline catch up. Every node converges on the same history.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::routing;

/// One recorded change. `node_*` is the 변경자 (who), `ts` the 변경시기 (when).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Unix seconds.
    pub ts: i64,
    pub node_id: String,
    pub node_name: String,
    /// "added" | "modified" | "deleted".
    pub action: String,
    pub path: String,
}

/// Stable cross-node identity for dedup. Coalescing keeps (node,path,action)
/// unique within any 5s window, so this tuple is effectively unique fleet-wide.
fn entry_id(e: &HistoryEntry) -> String {
    format!("{}|{}|{}|{}", e.node_id, e.ts, e.action, e.path)
}

/// Keep at most this many entries in memory (the on-disk log is unbounded).
const MAX_IN_MEMORY: usize = 4000;

/// Thread-safe shared change log.
#[derive(Clone)]
pub struct History {
    inner: Arc<Mutex<Inner>>,
    root: Arc<PathBuf>,
}

struct Inner {
    entries: Vec<HistoryEntry>,
    ids: HashSet<String>,
}

impl History {
    /// Create, loading any previously persisted entries.
    pub fn new(root: Arc<PathBuf>) -> Self {
        let entries = load(&root);
        let ids = entries.iter().map(entry_id).collect();
        History {
            inner: Arc::new(Mutex::new(Inner { entries, ids })),
            root,
        }
    }

    /// Record a LOCAL change (변경자 = this node). Returns the new entry so the
    /// caller can broadcast it, or `None` if it was coalesced / already known.
    pub fn record(
        &self,
        node_id: &str,
        node_name: &str,
        action: &str,
        path: &str,
    ) -> Option<HistoryEntry> {
        let ts = now_secs();
        let mut g = self.inner.lock().unwrap();
        // Coalesce rapid repeats (live text edits): same who/path/action within 5s.
        const COALESCE_SECS: i64 = 5;
        if let Some(last) = g.entries.last() {
            if last.node_id == node_id
                && last.path == path
                && last.action == action
                && ts - last.ts <= COALESCE_SECS
            {
                return None;
            }
        }
        let entry = HistoryEntry {
            ts,
            node_id: node_id.to_string(),
            node_name: node_name.to_string(),
            action: action.to_string(),
            path: path.to_string(),
        };
        if !self.insert_locked(&mut g, entry.clone()) {
            return None;
        }
        Some(entry)
    }

    /// Apply an entry learned from a peer. Returns true if it was new.
    pub fn apply_remote(&self, entry: HistoryEntry) -> bool {
        let mut g = self.inner.lock().unwrap();
        self.insert_locked(&mut g, entry)
    }

    /// The most recent `n` entries, newest first (for the GUI).
    pub fn recent(&self, n: usize) -> Vec<HistoryEntry> {
        let g = self.inner.lock().unwrap();
        let mut v: Vec<HistoryEntry> = g.entries.clone();
        v.sort_by(|a, b| b.ts.cmp(&a.ts));
        v.truncate(n);
        v
    }

    fn insert_locked(&self, g: &mut Inner, entry: HistoryEntry) -> bool {
        let id = entry_id(&entry);
        if !g.ids.insert(id) {
            return false; // already have it
        }
        append_line(&self.root, &entry);
        g.entries.push(entry);
        let len = g.entries.len();
        if len > MAX_IN_MEMORY {
            g.entries.drain(0..len - MAX_IN_MEMORY);
        }
        true
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn log_path(root: &std::path::Path) -> PathBuf {
    root.join(routing::MINISYNC_DIR).join("history.jsonl")
}

fn load(root: &std::path::Path) -> Vec<HistoryEntry> {
    let content = match std::fs::read_to_string(log_path(root)) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    content
        .lines()
        .filter_map(|l| serde_json::from_str::<HistoryEntry>(l).ok())
        .collect()
}

fn append_line(root: &std::path::Path, entry: &HistoryEntry) {
    use std::io::Write;
    let dir = root.join(routing::MINISYNC_DIR);
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(json) = serde_json::to_string(entry) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path(root))
        {
            let _ = writeln!(f, "{json}");
        }
    }
}
