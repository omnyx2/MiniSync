//! Watching the sync folder for changes, cross-platform.
//!
//! `notify` picks the right OS backend automatically: inotify on Linux,
//! FSEvents on macOS, ReadDirectoryChangesW on Windows. You get one API.
//!
//! Events are **debounced**: rapid successive events for the same path are
//! coalesced, and we only emit after the path has been quiet for `DEBOUNCE`.
//! This prevents double-sends (e.g. empty-file-create + content-write) and
//! avoids reading partial/intermediate file states.

use anyhow::Result;
use notify::event::{ModifyKind, RenameMode};
use notify::{EventKind, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// How long a path must be quiet before we emit it.
const DEBOUNCE: Duration = Duration::from_millis(300);
/// How often we wake to check pending paths (only while pending is non-empty).
const TICK: Duration = Duration::from_millis(50);

/// What kind of change the watcher detected.
#[derive(Debug, Clone)]
pub enum WatchEvent {
    /// File was created or modified — content should be synced.
    Changed(PathBuf),
    /// File was removed (or renamed away) — should be deleted on peers.
    Removed(PathBuf),
}

/// Internal: tracks the latest event kind per path during the debounce window.
#[derive(Clone, Copy)]
enum PendingKind {
    Changed,
    Removed,
}

struct Pending {
    kind: PendingKind,
    at: Instant,
}

/// Start watching `root` recursively. Returns a receiver that yields debounced
/// `WatchEvent`s for created/modified/removed files.
pub fn watch_folder(root: &Path) -> Result<Receiver<WatchEvent>> {
    let (tx, rx) = mpsc::channel::<WatchEvent>();
    let (raw_tx, raw_rx) = mpsc::channel();

    let mut watcher =
        notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                let _ = raw_tx.send(event);
            }
        })?;
    watcher.watch(root, RecursiveMode::Recursive)?;

    std::thread::spawn(move || {
        let _watcher = watcher; // keep alive
        let mut pending: HashMap<PathBuf, Pending> = HashMap::new();

        loop {
            // If nothing pending, block until the next event arrives.
            // If paths are pending, poll briefly so we can check readiness.
            let event = if pending.is_empty() {
                match raw_rx.recv() {
                    Ok(e) => Some(e),
                    Err(_) => break,
                }
            } else {
                match raw_rx.recv_timeout(TICK) {
                    Ok(e) => Some(e),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            };

            if let Some(ev) = event {
                accumulate(&ev, &mut pending);
            }
            // Drain any more that are immediately available.
            while let Ok(ev) = raw_rx.try_recv() {
                accumulate(&ev, &mut pending);
            }

            // Emit paths that have been stable for at least DEBOUNCE.
            let now = Instant::now();
            pending.retain(|path, entry| {
                if now.duration_since(entry.at) >= DEBOUNCE {
                    let evt = match entry.kind {
                        PendingKind::Changed => WatchEvent::Changed(path.clone()),
                        PendingKind::Removed => WatchEvent::Removed(path.clone()),
                    };
                    let _ = tx.send(evt);
                    false // remove from pending
                } else {
                    true // keep waiting
                }
            });
        }
    });

    Ok(rx)
}

/// Classify an OS event and upsert it into the pending map (last event wins
/// within the debounce window — so delete→recreate produces Changed, and
/// create→delete produces Removed).
fn accumulate(event: &notify::Event, pending: &mut HashMap<PathBuf, Pending>) {
    let now = Instant::now();

    match &event.kind {
        // Renames are Modify(Name(..)) in notify 6 — match before the general Modify arm.
        EventKind::Modify(ModifyKind::Name(mode)) => {
            match mode {
                RenameMode::From => {
                    // Old path is gone.
                    for path in &event.paths {
                        pending.insert(path.clone(), Pending { kind: PendingKind::Removed, at: now });
                    }
                }
                RenameMode::To => {
                    // New path appeared.
                    for path in &event.paths {
                        if path.is_file() {
                            pending.insert(path.clone(), Pending { kind: PendingKind::Changed, at: now });
                        }
                    }
                }
                RenameMode::Both => {
                    // paths[0] = old (gone), paths[1] = new (appeared).
                    if let Some(old) = event.paths.first() {
                        pending.insert(old.clone(), Pending { kind: PendingKind::Removed, at: now });
                    }
                    if let Some(new) = event.paths.get(1) {
                        if new.is_file() {
                            pending.insert(new.clone(), Pending { kind: PendingKind::Changed, at: now });
                        }
                    }
                }
                _ => {
                    // RenameMode::Any / Other — check existence to decide kind.
                    for path in &event.paths {
                        let kind = if path.is_file() {
                            PendingKind::Changed
                        } else {
                            PendingKind::Removed
                        };
                        pending.insert(path.clone(), Pending { kind, at: now });
                    }
                }
            }
        }
        EventKind::Create(_) | EventKind::Modify(_) => {
            for path in &event.paths {
                if path.is_file() {
                    pending.insert(path.clone(), Pending { kind: PendingKind::Changed, at: now });
                }
            }
        }
        EventKind::Remove(_) => {
            // File is already gone — can't check is_file(). Just record it.
            for path in &event.paths {
                pending.insert(path.clone(), Pending { kind: PendingKind::Removed, at: now });
            }
        }
        _ => {}
    }
}
