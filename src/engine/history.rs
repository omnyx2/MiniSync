//! Change history: a lightweight append-only log of *who* changed *what* and
//! *when*. No commit messages, no rollback — just an audit trail of sync activity
//! surfaced in the GUI. Persisted as JSON-lines at `.minisync/history.jsonl`.

use serde::{Deserialize, Serialize};
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

/// Keep at most this many entries in memory (the on-disk log is unbounded).
const MAX_IN_MEMORY: usize = 2000;

/// Thread-safe change log shared across the engine.
#[derive(Clone)]
pub struct History {
    entries: Arc<Mutex<Vec<HistoryEntry>>>,
    root: Arc<PathBuf>,
}

impl History {
    /// Create, loading any previously persisted entries.
    pub fn new(root: Arc<PathBuf>) -> Self {
        let entries = load(&root);
        History {
            entries: Arc::new(Mutex::new(entries)),
            root,
        }
    }

    /// Record a change. `action` is "added" / "modified" / "deleted".
    pub fn record(&self, node_id: &str, node_name: &str, action: &str, path: &str) {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let mut v = self.entries.lock().unwrap();
        // Coalesce rapid repeats (e.g. live text edits): drop a duplicate of the
        // last entry — same who/path/action — within a short window.
        const COALESCE_SECS: i64 = 5;
        if let Some(last) = v.last() {
            if last.node_id == node_id
                && last.path == path
                && last.action == action
                && ts - last.ts <= COALESCE_SECS
            {
                return;
            }
        }
        let entry = HistoryEntry {
            ts,
            node_id: node_id.to_string(),
            node_name: node_name.to_string(),
            action: action.to_string(),
            path: path.to_string(),
        };
        append_line(&self.root, &entry);
        v.push(entry);
        let len = v.len();
        if len > MAX_IN_MEMORY {
            v.drain(0..len - MAX_IN_MEMORY);
        }
    }

    /// The most recent `n` entries, newest first (for the GUI).
    pub fn recent(&self, n: usize) -> Vec<HistoryEntry> {
        let v = self.entries.lock().unwrap();
        v.iter().rev().take(n).cloned().collect()
    }
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
