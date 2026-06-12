//! File browser panel: folder-navigable catalog view with Download buttons.
//!
//! Entries are stored as flat relative paths (e.g. `docs/spec/menu.csv`). This
//! panel renders them like a file manager: only the items directly inside the
//! current directory are shown, sub-folders are clickable to descend into, and
//! a breadcrumb bar walks back up.

use eframe::egui;
use std::collections::BTreeMap;
use std::sync::mpsc::Sender;

use crate::catalog::{CatalogEntry, FileLocation};
use crate::config::SyncMode;
use crate::engine::GuiCommand;

/// Aggregated info about a sub-folder directly under the current directory.
struct FolderRow {
    name: String,
    file_count: usize,
    total_size: u64,
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
    current_dir: &mut String,
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
                        total_size: 0,
                    });
                row.file_count += 1;
                row.total_size += entry.size;
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

    egui::ScrollArea::vertical().show(ui, |ui| {
        egui::Grid::new("file_grid")
            .striped(true)
            .min_col_width(60.0)
            .show(ui, |ui| {
                // Header
                ui.strong("Name");
                ui.strong("Size");
                ui.strong("Mode");
                ui.strong("Location");
                ui.strong("Action");
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
                    ui.label("");
                    ui.label(format!("{} item(s)", folder.file_count));
                    ui.label("");
                    ui.end_row();
                }

                // Files in this directory.
                for entry in &files {
                    let name = file_name_of(&entry.path);
                    ui.label(format!("📄 {name}"));
                    ui.label(format_size(entry.size));
                    ui.label(mode_label(entry.sync_mode));
                    ui.label(location_label(&entry.location, self_node_name));

                    // Action: selective-sync toggle.
                    //  - CRDT (text/code) files always sync → no toggle.
                    //  - File-lane references: Download (if not here) / Remove (if here).
                    if crate::routing::lane_for(&entry.path) == crate::routing::Lane::Crdt {
                        ui.weak("auto-sync");
                    } else {
                        match &entry.location {
                            FileLocation::Remote { .. } => {
                                if ui
                                    .button("⬇ Download")
                                    .on_hover_text("Download a copy onto this device")
                                    .clicked()
                                {
                                    let _ = commands_tx
                                        .send(GuiCommand::Download(entry.path.clone()));
                                }
                            }
                            // Local or Both — we hold a copy; offer to drop it.
                            _ => {
                                if ui
                                    .button("🗑 Remove")
                                    .on_hover_text(
                                        "Remove from THIS device only (kept on peers, re-downloadable)",
                                    )
                                    .clicked()
                                {
                                    let _ = commands_tx
                                        .send(GuiCommand::RemoveLocal(entry.path.clone()));
                                }
                            }
                        }
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

fn mode_label(mode: SyncMode) -> &'static str {
    match mode {
        SyncMode::FullCopy => "full",
        SyncMode::Reference => "ref",
    }
}

fn location_label(loc: &FileLocation, self_node_name: &str) -> String {
    match loc {
        FileLocation::Local => self_node_name.to_string(),
        FileLocation::Remote { owners } => {
            let names: Vec<&str> = owners.iter().map(|o| o.node_name.as_str()).collect();
            if names.is_empty() {
                "remote".to_string()
            } else {
                names.join(", ")
            }
        }
        FileLocation::Both { owners } => {
            let mut names = vec![self_node_name];
            for o in owners {
                names.push(&o.node_name);
            }
            names.join(", ")
        }
    }
}
