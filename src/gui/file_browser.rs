//! File browser panel: folder-navigable catalog view with Download buttons.
//!
//! Entries are stored as flat relative paths (e.g. `docs/spec/menu.csv`). This
//! panel renders them like a file manager: only the items directly inside the
//! current directory are shown, sub-folders are clickable to descend into, and
//! a breadcrumb bar walks back up.

use eframe::egui;
use std::collections::{BTreeMap, HashSet};
use std::sync::mpsc::Sender;

use crate::catalog::{CatalogEntry, FileLocation, NodeInfo};
use crate::engine::GuiCommand;

/// Aggregated info about a sub-folder directly under the current directory.
struct FolderRow {
    name: String,
    file_count: usize,
    /// How many of those files are held locally (synced onto this device).
    held_count: usize,
    total_size: u64,
}

/// A destructive action awaiting user confirmation in a modal. The panel only
/// records the intent; `GuiApp` renders the confirm dialog and issues the command.
pub struct PendingConfirm {
    pub path: String,
    /// True when this is a "remove my only copy" → effectively a network delete,
    /// so the dialog warns it's the last copy.
    pub last_copy: bool,
}


/// Render the file browser panel.
///
/// `current_dir` is the relative path of the folder currently being viewed
/// (empty string = sync root). It is mutated when the user navigates.
pub fn file_browser_panel(
    ui: &mut egui::Ui,
    entries: &[CatalogEntry],
    commands_tx: &Sender<GuiCommand>,
    self_node_name: &str,
    self_node_id: &str,
    online_peers: &HashSet<String>,
    current_dir: &mut String,
    pending: &mut Option<PendingConfirm>,
) {
    ui.heading("File Browser");

    // Breadcrumb bar: root / seg1 / seg2 ...
    ui.horizontal(|ui| {
        if ui.button("🏠 root").clicked() {
            current_dir.clear();
        }
        // Own the segments so the cumulative prefix doesn't keep `current_dir`
        // borrowed while we re-assign it on a click.
        let segments: Vec<String> = if current_dir.is_empty() {
            Vec::new()
        } else {
            current_dir.split('/').map(|s| s.to_string()).collect()
        };
        let mut acc = String::new();
        for seg in &segments {
            ui.label("/");
            if acc.is_empty() {
                acc = seg.clone();
            } else {
                acc = format!("{acc}/{seg}");
            }
            if ui.button(seg).clicked() {
                *current_dir = acc.clone();
            }
        }
    });
    ui.separator();

    // Split entries into sub-folders and files that live directly in current_dir.
    let prefix = if current_dir.is_empty() {
        String::new()
    } else {
        format!("{current_dir}/")
    };

    let mut folders: BTreeMap<String, FolderRow> = BTreeMap::new();
    let mut files: Vec<&CatalogEntry> = Vec::new();

    for entry in entries {
        if !prefix.is_empty() && !entry.path.starts_with(&prefix) {
            continue;
        }
        let rel = &entry.path[prefix.len()..];
        if rel.is_empty() {
            continue;
        }
        match rel.find('/') {
            Some(slash) => {
                // Lives inside a sub-folder of the current directory.
                let folder_name = &rel[..slash];
                let row = folders
                    .entry(folder_name.to_string())
                    .or_insert_with(|| FolderRow {
                        name: folder_name.to_string(),
                        file_count: 0,
                        held_count: 0,
                        total_size: 0,
                    });
                row.file_count += 1;
                row.total_size += entry.size;
                if matches!(
                    entry.location,
                    FileLocation::Local | FileLocation::Both { .. }
                ) {
                    row.held_count += 1;
                }
            }
            None => {
                // A file directly in the current directory.
                files.push(entry);
            }
        }
    }

    if folders.is_empty() && files.is_empty() {
        ui.weak("(empty folder)");
        return;
    }

    egui::ScrollArea::both().show(ui, |ui| {
        egui::Grid::new("file_grid")
            .striped(true)
            .min_col_width(60.0)
            .show(ui, |ui| {
                // Header
                ui.strong("Name");
                ui.strong("Size");
                ui.strong("Location");
                ui.strong("Available");
                ui.strong("Sync");
                ui.strong("Delete");
                ui.end_row();

                // Folders first, clickable to descend.
                for folder in folders.values() {
                    if ui
                        .button(format!("📁 {}", folder.name))
                        .on_hover_text("Open folder")
                        .clicked()
                    {
                        *current_dir = if current_dir.is_empty() {
                            folder.name.clone()
                        } else {
                            format!("{current_dir}/{}", folder.name)
                        };
                    }
                    ui.label(format_size(folder.total_size));
                    ui.label(format!("{} item(s)", folder.file_count));
                    ui.label(""); // Available: n/a for folders
                    // Sync: how many sub-items are held here. Green when all synced.
                    let all = folder.file_count > 0 && folder.held_count == folder.file_count;
                    let txt = format!("{}/{} synced", folder.held_count, folder.file_count);
                    if all {
                        ui.colored_label(egui::Color32::from_rgb(60, 160, 60), txt);
                    } else {
                        ui.weak(txt);
                    }
                    ui.label(""); // no folder-level delete (avoid mass deletion)
                    ui.end_row();
                }

                // Files in this directory.
                for entry in &files {
                    let name = file_name_of(&entry.path);
                    ui.label(format!("📄 {name}"));
                    ui.label(format_size(entry.size));
                    location_cell(ui, entry, self_node_id, self_node_name);

                    let is_crdt =
                        crate::routing::lane_for(&entry.path) == crate::routing::Lane::Crdt;
                    let i_hold = matches!(
                        entry.location,
                        FileLocation::Local | FileLocation::Both { .. }
                    );
                    // Other nodes that hold a copy (excludes us). If we hold and this
                    // is 0, we're the last copy → Remove would destroy the file.
                    let other_holders = match &entry.location {
                        FileLocation::Remote { owners } | FileLocation::Both { owners } => {
                            owners.len()
                        }
                        FileLocation::Local => 0,
                    };
                    let green = egui::Color32::from_rgb(60, 160, 60);
                    let red = egui::Color32::from_rgb(200, 90, 90);

                    // A file is fetchable only if WE already hold it, or some node
                    // that holds it is currently online. If every holder is offline,
                    // it can't be downloaded.
                    let holder_online = match &entry.location {
                        FileLocation::Remote { owners } | FileLocation::Both { owners } => {
                            owners.iter().any(|o| online_peers.contains(&o.node_id))
                        }
                        FileLocation::Local => false,
                    };
                    let available = i_hold || holder_online;

                    // Available column.
                    if i_hold {
                        ui.colored_label(green, "✓ here");
                    } else if available {
                        ui.colored_label(green, "● online");
                    } else {
                        ui.colored_label(red, "unavailable")
                            .on_hover_text("Every device that has this file is offline");
                    }

                    // Sync column: a toggle — ON = keep a local copy, auto-updated
                    // (green "auto-sync"); OFF = reference only. CRDT/text files are
                    // always synced (shown as a locked green status, not a toggle).
                    if is_crdt {
                        ui.colored_label(green, "auto-sync").on_hover_text(
                            "Text/CRDT file — auto-merged and kept on every device; can't disable",
                        );
                    } else {
                        let mut on = i_hold;
                        let label = if on {
                            egui::RichText::new("auto-sync").color(green)
                        } else {
                            egui::RichText::new("off").weak()
                        };
                        // Can't turn ON (download) if no holder is online.
                        let can_toggle = on || available;
                        let hover = if !can_toggle {
                            "Can't download — every device with this file is offline"
                        } else if on {
                            "Synced on this device — toggle off to keep only a reference"
                        } else {
                            "Reference only — toggle on to download & keep synced here"
                        };
                        let resp = ui
                            .add_enabled(can_toggle, egui::Checkbox::new(&mut on, label))
                            .on_hover_text(hover);
                        if resp.changed() {
                            if on {
                                let _ = commands_tx.send(GuiCommand::Download(entry.path.clone()));
                            } else if other_holders == 0 {
                                *pending = Some(PendingConfirm {
                                    path: entry.path.clone(),
                                    last_copy: true,
                                });
                            } else {
                                let _ =
                                    commands_tx.send(GuiCommand::RemoveLocal(entry.path.clone()));
                            }
                        }
                    }

                    // Delete column: network-wide delete (distinct from Remove).
                    // Red icon on a normal button — signals danger without a heavy fill.
                    let del = egui::Button::new(
                        egui::RichText::new("🗑").color(egui::Color32::from_rgb(210, 75, 75)),
                    );
                    if ui
                        .add(del)
                        .on_hover_text("Delete everywhere — remove from ALL devices (cannot be undone)")
                        .clicked()
                    {
                        *pending = Some(PendingConfirm {
                            path: entry.path.clone(),
                            last_copy: false,
                        });
                    }
                    ui.end_row();
                }
            });
    });
}

/// Last path segment (the bare file name) of a relative path.
fn file_name_of(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

/// Render the Location cell: the file's ORIGIN (creator) plus the total holder
/// count, with a dropdown listing every node that currently holds a copy. If the
/// origin no longer keeps a local copy it's marked "(no copy)".
fn location_cell(ui: &mut egui::Ui, entry: &CatalogEntry, self_id: &str, self_name: &str) {
    let (i_hold, owners): (bool, &[NodeInfo]) = match &entry.location {
        FileLocation::Local => (true, &[]),
        FileLocation::Remote { owners } => (false, owners.as_slice()),
        FileLocation::Both { owners } => (true, owners.as_slice()),
    };

    // Every node that currently holds a copy (self first, if we do).
    let mut holders: Vec<String> = Vec::new();
    if i_hold {
        holders.push(format!("{self_name} (this device)"));
    }
    holders.extend(owners.iter().map(|o| o.node_name.clone()));
    let count = holders.len();

    let origin_txt = match &entry.origin {
        Some(o) => {
            let origin_holds =
                (i_hold && o.node_id == self_id) || owners.iter().any(|h| h.node_id == o.node_id);
            if origin_holds {
                o.node_name.clone()
            } else {
                format!("{} (no copy)", o.node_name)
            }
        }
        // Legacy entry with no recorded origin → fall back to first holder.
        None => holders.first().cloned().unwrap_or_else(|| "unknown".to_string()),
    };

    ui.menu_button(format!("{origin_txt} ({count})"), |ui| {
        if let Some(o) = &entry.origin {
            ui.label(format!("Origin: {}", o.node_name));
            ui.separator();
        }
        ui.label("Holders:");
        if holders.is_empty() {
            ui.weak("(none — no copy anywhere)");
        } else {
            for h in &holders {
                ui.label(format!("• {h}"));
            }
        }
    });
}
