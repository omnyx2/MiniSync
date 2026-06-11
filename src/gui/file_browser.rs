//! File browser panel: catalog table with Download buttons for remote files.

use eframe::egui;
use std::sync::mpsc::Sender;

use crate::catalog::{CatalogEntry, FileLocation};
use crate::config::SyncMode;
use crate::engine::GuiCommand;

/// Render the file browser panel.
pub fn file_browser_panel(
    ui: &mut egui::Ui,
    entries: &[CatalogEntry],
    commands_tx: &Sender<GuiCommand>,
) {
    ui.heading("File Browser");
    ui.separator();

    egui::ScrollArea::vertical().show(ui, |ui| {
        egui::Grid::new("file_grid")
            .striped(true)
            .min_col_width(60.0)
            .show(ui, |ui| {
                // Header
                ui.strong("Path");
                ui.strong("Size");
                ui.strong("Mode");
                ui.strong("Location");
                ui.strong("Action");
                ui.end_row();

                for entry in entries {
                    ui.label(&entry.path);
                    ui.label(format_size(entry.size));
                    ui.label(mode_label(entry.sync_mode));
                    ui.label(location_label(&entry.location));

                    match &entry.location {
                        FileLocation::Remote { .. } => {
                            if ui.button("Download").clicked() {
                                let _ = commands_tx.send(GuiCommand::Download(entry.path.clone()));
                            }
                        }
                        _ => {
                            ui.label("");
                        }
                    }
                    ui.end_row();
                }
            });
    });
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

fn location_label(loc: &FileLocation) -> &'static str {
    match loc {
        FileLocation::Local => "local",
        FileLocation::Remote { .. } => "remote",
        FileLocation::Both { .. } => "both",
    }
}
