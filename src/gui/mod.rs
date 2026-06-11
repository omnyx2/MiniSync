//! Desktop GUI for minisync using egui/eframe.

pub mod file_browser;
pub mod peers_panel;
pub mod settings_panel;
pub mod state;

use eframe::egui;
use std::path::Path;

use self::settings_panel::SettingsState;
use self::state::GuiBridge;
use crate::engine::EngineEvent;

/// Main GUI application.
pub struct GuiApp {
    bridge: GuiBridge,
    show_settings: bool,
    settings_state: SettingsState,
    status_message: String,
    /// True while files are being hovered over the window.
    hovering_files: bool,
    /// Relative path of the folder currently shown in the file browser
    /// (empty string = sync root).
    current_dir: String,
}

impl GuiApp {
    pub fn new(bridge: GuiBridge) -> Self {
        GuiApp {
            bridge,
            show_settings: false,
            settings_state: SettingsState::default(),
            status_message: String::new(),
            hovering_files: false,
            current_dir: String::new(),
        }
    }

    /// Drain pending engine events.
    fn poll_events(&mut self) {
        while let Ok(event) = self.bridge.events_rx.try_recv() {
            match event {
                EngineEvent::CatalogUpdated => {}
                EngineEvent::PeerConnected { remote_id } => {
                    self.status_message = format!("Peer {remote_id} connected");
                }
                EngineEvent::PeerDisconnected { remote_id } => {
                    self.status_message = format!("Peer {remote_id} disconnected");
                }
                EngineEvent::Error(msg) => {
                    self.status_message = format!("Error: {msg}");
                }
            }
        }
    }

    /// Handle drag-and-drop: copy dropped files into the sync root.
    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        self.hovering_files = !ctx.input(|i| i.raw.hovered_files.is_empty());

        let dropped: Vec<egui::DroppedFile> = ctx.input(|i| i.raw.dropped_files.clone());
        if dropped.is_empty() {
            return;
        }

        let root = self.bridge.root.as_path();
        let mut imported = 0usize;

        for file in &dropped {
            if let Some(ref source_path) = file.path {
                if source_path.is_file() {
                    match import_file(source_path, root) {
                        Ok(rel) => {
                            imported += 1;
                            println!("[gui] imported: {rel}");
                        }
                        Err(e) => {
                            eprintln!("[gui] drop error for {:?}: {e}", source_path);
                        }
                    }
                } else if source_path.is_dir() {
                    match import_dir(source_path, root) {
                        Ok(count) => {
                            imported += count;
                            println!(
                                "[gui] imported directory {:?} ({count} files)",
                                source_path
                            );
                        }
                        Err(e) => {
                            eprintln!("[gui] drop error for dir {:?}: {e}", source_path);
                        }
                    }
                }
            } else if let Some(ref bytes) = file.bytes {
                let name = file.name.clone();
                if !name.is_empty() {
                    let dest = root.join(&name);
                    if let Some(parent) = dest.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    match std::fs::write(&dest, bytes.as_ref()) {
                        Ok(()) => {
                            imported += 1;
                            println!("[gui] imported (bytes): {name}");
                        }
                        Err(e) => {
                            eprintln!("[gui] drop write error for {name}: {e}");
                        }
                    }
                }
            }
        }

        if imported > 0 {
            self.status_message = format!("Imported {imported} file(s)");
        }
    }
}

/// Copy a single file into the sync root, preserving its filename.
fn import_file(source: &Path, root: &Path) -> Result<String, std::io::Error> {
    let filename = source
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no filename"))?;
    let dest = root.join(filename);
    std::fs::copy(source, &dest)?;
    Ok(filename.to_string_lossy().to_string())
}

/// Recursively copy a directory into the sync root.
fn import_dir(source: &Path, root: &Path) -> Result<usize, std::io::Error> {
    let dir_name = source
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no dir name"))?;
    let dest_base = root.join(dir_name);
    let mut count = 0;

    for entry in walkdir::WalkDir::new(source)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let rel = match entry.path().strip_prefix(source) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let dest = dest_base.join(rel);

        if entry.file_type().is_dir() {
            let _ = std::fs::create_dir_all(&dest);
        } else if entry.file_type().is_file() {
            if let Some(parent) = dest.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::copy(entry.path(), &dest)?;
            count += 1;
        }
    }
    Ok(count)
}

/// Open a path in the native file manager (Finder on macOS).
fn open_in_file_manager(path: &Path) {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(path).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("explorer").arg(path).spawn();
    }
}

/// Show a native folder picker dialog. Returns the selected path, or None.
fn pick_folder(current: &Path) -> Option<std::path::PathBuf> {
    rfd::FileDialog::new()
        .set_title("Choose sync folder")
        .set_directory(current)
        .pick_folder()
}

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_events();
        self.handle_dropped_files(ctx);

        let peers = self.bridge.registry.peer_list();
        let entries = self.bridge.catalog.snapshot();
        let root_display = self.bridge.root.display().to_string();

        // Top panel
        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("minisync");
                ui.separator();
                ui.label(format!("Peers: {} connected", peers.len()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Settings").clicked() {
                        self.show_settings = !self.show_settings;
                    }
                });
            });

            // Sync folder path bar
            ui.horizontal(|ui| {
                ui.label("Sync folder:");
                ui.monospace(&root_display);
                if ui.small_button("Open").on_hover_text("Open in file manager").clicked() {
                    open_in_file_manager(&self.bridge.root);
                }
                if ui.small_button("Change...").on_hover_text("Choose a different sync folder (requires restart)").clicked() {
                    if let Some(new_path) = pick_folder(&self.bridge.root) {
                        // Save new folder to global app config
                        let mut app_cfg = crate::config::app::AppConfig::load()
                            .unwrap_or_default();
                        app_cfg.sync_folder = new_path.display().to_string();
                        if let Err(e) = app_cfg.save() {
                            self.status_message = format!("Failed to save settings: {e}");
                        } else {
                            self.status_message = format!(
                                "Saved! Restart minisync to use: {}",
                                new_path.display()
                            );
                        }
                    }
                }
            });
        });

        // Bottom status bar
        egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let local_count = entries
                    .iter()
                    .filter(|e| {
                        matches!(
                            e.location,
                            crate::catalog::FileLocation::Local
                                | crate::catalog::FileLocation::Both { .. }
                        )
                    })
                    .count();
                let ref_count = entries
                    .iter()
                    .filter(|e| {
                        matches!(e.location, crate::catalog::FileLocation::Remote { .. })
                    })
                    .count();
                ui.label(format!("Files: {}  Refs: {}", local_count, ref_count));
                if !self.status_message.is_empty() {
                    ui.separator();
                    ui.label(&self.status_message);
                }
            });
        });

        // Left panel: peers
        egui::SidePanel::left("peers_panel")
            .default_width(180.0)
            .show(ctx, |ui| {
                peers_panel::peers_panel(ui, &peers);
            });

        // Settings window
        if self.show_settings {
            egui::Window::new("Settings")
                .open(&mut self.show_settings)
                .show(ctx, |ui| {
                    settings_panel::settings_panel(
                        ui,
                        &self.bridge.config,
                        &self.bridge.commands_tx,
                        &mut self.settings_state,
                        &mut self.bridge.node_name,
                        &mut self.status_message,
                    );
                });
        }

        // Central panel: file browser
        egui::CentralPanel::default().show(ctx, |ui| {
            file_browser::file_browser_panel(
                ui,
                &entries,
                &self.bridge.commands_tx,
                &self.bridge.node_name,
                &mut self.current_dir,
            );
        });

        // Drop overlay
        if self.hovering_files {
            egui::Area::new(egui::Id::new("drop_overlay"))
                .fixed_pos(egui::Pos2::ZERO)
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    let screen = ctx.screen_rect();
                    let painter = ui.painter();

                    painter.rect_filled(screen, 0.0, egui::Color32::from_black_alpha(160));
                    painter.text(
                        screen.center(),
                        egui::Align2::CENTER_CENTER,
                        "Drop files here to import",
                        egui::FontId::proportional(28.0),
                        egui::Color32::WHITE,
                    );
                    painter.rect_stroke(
                        screen.shrink(20.0),
                        12.0,
                        egui::Stroke::new(3.0, egui::Color32::WHITE),
                    );
                });
        }

        ctx.request_repaint_after(std::time::Duration::from_millis(500));
    }
}

/// Run the GUI. This blocks until the window is closed.
pub fn run_gui(bridge: GuiBridge) -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([900.0, 600.0])
            .with_min_inner_size([600.0, 400.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "minisync",
        options,
        Box::new(|_cc| Ok(Box::new(GuiApp::new(bridge)))),
    )
}
