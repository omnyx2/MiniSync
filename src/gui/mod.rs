//! Desktop GUI for minisync using egui/eframe.

pub mod file_browser;
pub mod peers_panel;
pub mod settings_panel;
pub mod state;

use eframe::egui;
use std::path::Path;

use self::file_browser::PendingConfirm;
use self::settings_panel::SettingsState;
use self::state::GuiBridge;
use crate::engine::{EngineEvent, GuiCommand};

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
    /// Conflict notifications (newest last): "path ← from". Concurrent edits keep
    /// the remote copy as `path.conflict-<peer>`; this surfaces it to the user.
    conflicts: Vec<String>,
    /// Whether the conflict list window is open.
    show_conflicts: bool,
    /// A destructive delete awaiting confirmation (set by the file browser).
    pending_confirm: Option<PendingConfirm>,
    /// Whether the change-history window is open.
    show_history: bool,
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
            conflicts: Vec::new(),
            show_conflicts: false,
            pending_confirm: None,
            show_history: false,
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
                EngineEvent::Conflict { path, from } => {
                    self.status_message = format!("⚠ Conflict: {path} (from {from})");
                    self.conflicts.push(format!("{path}  ←  {from}"));
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

/// Human-readable relative time, e.g. "just now", "5m ago", "2h ago", "3d ago".
fn format_ago(now: i64, then: i64) -> String {
    let secs = (now - then).max(0);
    if secs < 10 {
        "just now".to_string()
    } else if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
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
                    if ui.button("History").clicked() {
                        self.show_history = !self.show_history;
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

        // Conflict warning banner (only when there are unresolved conflicts).
        if !self.conflicts.is_empty() {
            egui::TopBottomPanel::top("conflict_banner")
                .frame(egui::Frame::none().fill(egui::Color32::from_rgb(120, 40, 40)).inner_margin(6.0))
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.colored_label(
                            egui::Color32::WHITE,
                            format!("⚠ {} conflict(s) detected", self.conflicts.len()),
                        );
                        if ui.button("View").clicked() {
                            self.show_conflicts = true;
                        }
                        if ui.button("Dismiss").clicked() {
                            self.conflicts.clear();
                            self.show_conflicts = false;
                        }
                    });
                });
        }

        // Conflict list window.
        if self.show_conflicts {
            egui::Window::new("Conflicts")
                .open(&mut self.show_conflicts)
                .show(ctx, |ui| {
                    ui.label("Concurrent edits — the remote copy was kept alongside as");
                    ui.monospace("<file>.conflict-<peer>");
                    ui.separator();
                    egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                        for c in &self.conflicts {
                            ui.label(c);
                        }
                    });
                });
        }

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

        // Change-history window: who changed what, when.
        if self.show_history {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let entries = self.bridge.engine.history.recent(300);
            egui::Window::new("Change history")
                .open(&mut self.show_history)
                .default_width(460.0)
                .show(ctx, |ui| {
                    if entries.is_empty() {
                        ui.weak("No changes recorded yet.");
                        return;
                    }
                    egui::ScrollArea::vertical().max_height(400.0).show(ui, |ui| {
                        egui::Grid::new("history_grid")
                            .striped(true)
                            .num_columns(4)
                            .show(ui, |ui| {
                                ui.strong("When");
                                ui.strong("Who");
                                ui.strong("Action");
                                ui.strong("File");
                                ui.end_row();
                                for e in &entries {
                                    ui.label(format_ago(now, e.ts));
                                    ui.label(&e.node_name);
                                    ui.label(&e.action);
                                    ui.label(&e.path);
                                    ui.end_row();
                                }
                            });
                    });
                });
        }

        // Central panel: file browser
        egui::CentralPanel::default().show(ctx, |ui| {
            file_browser::file_browser_panel(
                ui,
                &entries,
                &self.bridge.commands_tx,
                &self.bridge.node_name,
                &self.bridge.engine.peer_id,
                &mut self.current_dir,
                &mut self.pending_confirm,
            );
        });

        // Destructive-delete confirmation modal.
        if let Some(pc) = &self.pending_confirm {
            let path = pc.path.clone();
            let last_copy = pc.last_copy;
            let mut decision: Option<bool> = None; // Some(true)=delete, Some(false)=cancel
            egui::Window::new("⚠ Confirm delete")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ctx, |ui| {
                    let red = egui::Color32::from_rgb(200, 80, 80);
                    if last_copy {
                        ui.colored_label(red, "This is the LAST copy.");
                        ui.label(format!(
                            "Removing \"{path}\" deletes it from the ENTIRE network. This cannot be undone."
                        ));
                    } else {
                        ui.colored_label(red, "Delete from ALL devices");
                        ui.label(format!(
                            "\"{path}\" will be deleted everywhere. This cannot be undone."
                        ));
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            decision = Some(false);
                        }
                        if ui.button("Delete everywhere").clicked() {
                            decision = Some(true);
                        }
                    });
                });
            match decision {
                Some(true) => {
                    let _ = self.bridge.commands_tx.send(GuiCommand::DeleteEverywhere(path));
                    self.pending_confirm = None;
                }
                Some(false) => self.pending_confirm = None,
                None => {}
            }
        }

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

        // Engine events now drive repaints via the repaint hook, so this is
        // just an idle safety net (covers anything not wired through notify_gui).
        ctx.request_repaint_after(std::time::Duration::from_secs(1));
    }
}

/// Load a system CJK font so Korean/CJK filenames render instead of showing as
/// tofu boxes (egui's bundled fonts are Latin-only). Tries known per-OS paths and
/// installs the first one found as a fallback for both font families.
fn install_cjk_font(ctx: &egui::Context) {
    #[cfg(target_os = "macos")]
    let candidates: &[&str] = &[
        "/System/Library/Fonts/Supplemental/AppleGothic.ttf",
        "/System/Library/Fonts/AppleSDGothicNeo.ttc",
    ];
    #[cfg(target_os = "windows")]
    let candidates: &[&str] = &[
        "C:\\Windows\\Fonts\\malgun.ttf",
        "C:\\Windows\\Fonts\\gulim.ttc",
        "C:\\Windows\\Fonts\\msgothic.ttc",
    ];
    #[cfg(target_os = "linux")]
    let candidates: &[&str] = &[
        "/usr/share/fonts/truetype/nanum/NanumGothic.ttf",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
    ];
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    let candidates: &[&str] = &[];

    let Some(bytes) = candidates
        .iter()
        .find_map(|p| std::fs::read(p).ok())
    else {
        eprintln!("[gui] no system CJK font found; non-Latin names may not render");
        return;
    };

    let mut fonts = egui::FontDefinitions::default();
    fonts
        .font_data
        .insert("cjk".to_owned(), egui::FontData::from_owned(bytes));
    // Append as the lowest-priority fallback for both families so Latin glyphs
    // keep egui's default look and CJK fills the gaps.
    for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(fam).or_default().push("cjk".to_owned());
    }
    ctx.set_fonts(fonts);
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
        Box::new(|cc| {
            // Load a CJK font so non-Latin filenames render.
            install_cjk_font(&cc.egui_ctx);
            // Install a repaint hook so engine events (local edits, files
            // arriving from peers, peer connect/disconnect) wake the window
            // instantly instead of waiting for the idle repaint timer.
            let ctx = cc.egui_ctx.clone();
            let _ = bridge
                .engine
                .repaint
                .set(Box::new(move || ctx.request_repaint()));
            Ok(Box::new(GuiApp::new(bridge)))
        }),
    )
}
