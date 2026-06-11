//! Settings panel: edit sync rules (pattern → mode).

use eframe::egui;
use std::sync::mpsc::Sender;
use std::sync::{Arc, RwLock};

use crate::config::app::AppConfig;
use crate::config::{SyncConfig, SyncMode, SyncRule};
use crate::engine::GuiCommand;

/// Mutable state for the settings editor within the GUI frame.
pub struct SettingsState {
    pub new_pattern: String,
    pub new_mode: SyncMode,
    pub editing: bool,
    pub node_name_edit: String,
    pub node_name_initialized: bool,
}

impl Default for SettingsState {
    fn default() -> Self {
        SettingsState {
            new_pattern: String::new(),
            new_mode: SyncMode::Reference,
            editing: false,
            node_name_edit: String::new(),
            node_name_initialized: false,
        }
    }
}

/// Render the settings panel.
pub fn settings_panel(
    ui: &mut egui::Ui,
    config: &Arc<RwLock<SyncConfig>>,
    commands_tx: &Sender<GuiCommand>,
    state: &mut SettingsState,
    node_name: &mut String,
    status_message: &mut String,
) {
    ui.heading("Settings");
    ui.separator();

    // Node name editor
    if !state.node_name_initialized {
        state.node_name_edit = node_name.clone();
        state.node_name_initialized = true;
    }

    ui.horizontal(|ui| {
        ui.label("Node name:");
        let response = ui.text_edit_singleline(&mut state.node_name_edit);
        if response.lost_focus()
            && ui.input(|i| i.key_pressed(egui::Key::Enter))
            && !state.node_name_edit.is_empty()
            && state.node_name_edit != *node_name
        {
            let new_name = state.node_name_edit.clone();
            *node_name = new_name.clone();
            // Save to AppConfig
            let mut app_cfg = AppConfig::load().unwrap_or_default();
            app_cfg.node_name = new_name.clone();
            if let Err(e) = app_cfg.save() {
                *status_message = format!("Failed to save node name: {e}");
            } else {
                *status_message = format!("Node name changed to: {new_name}");
            }
        }
        if ui.small_button("Save").clicked()
            && !state.node_name_edit.is_empty()
            && state.node_name_edit != *node_name
        {
            let new_name = state.node_name_edit.clone();
            *node_name = new_name.clone();
            let mut app_cfg = AppConfig::load().unwrap_or_default();
            app_cfg.node_name = new_name.clone();
            if let Err(e) = app_cfg.save() {
                *status_message = format!("Failed to save node name: {e}");
            } else {
                *status_message = format!("Node name changed to: {new_name}");
            }
        }
    });

    ui.separator();

    let cfg = config.read().unwrap().clone();

    // Default mode
    ui.horizontal(|ui| {
        ui.label("Default mode:");
        ui.label(match cfg.default_mode {
            SyncMode::FullCopy => "full_copy",
            SyncMode::Reference => "reference",
        });
    });

    ui.separator();
    ui.strong("Rules (first match wins):");

    // Existing rules
    let mut rules_changed = false;
    let mut new_rules = cfg.rules.clone();
    let mut to_remove: Option<usize> = None;

    for (i, rule) in cfg.rules.iter().enumerate() {
        ui.horizontal(|ui| {
            ui.label(format!("{}  →  {:?}", rule.pattern, rule.mode));
            if ui.small_button("X").clicked() {
                to_remove = Some(i);
            }
        });
    }

    if let Some(idx) = to_remove {
        new_rules.remove(idx);
        rules_changed = true;
    }

    ui.separator();

    // Add new rule
    ui.horizontal(|ui| {
        ui.label("Pattern:");
        ui.text_edit_singleline(&mut state.new_pattern);
    });

    ui.horizontal(|ui| {
        ui.label("Mode:");
        ui.radio_value(&mut state.new_mode, SyncMode::FullCopy, "full_copy");
        ui.radio_value(&mut state.new_mode, SyncMode::Reference, "reference");
    });

    if ui.button("Add Rule").clicked() && !state.new_pattern.is_empty() {
        new_rules.push(SyncRule {
            pattern: state.new_pattern.clone(),
            mode: state.new_mode,
        });
        state.new_pattern.clear();
        rules_changed = true;
    }

    if rules_changed {
        let new_config = SyncConfig {
            default_mode: cfg.default_mode,
            rules: new_rules,
        };
        let _ = commands_tx.send(GuiCommand::UpdateConfig(new_config));
    }
}
