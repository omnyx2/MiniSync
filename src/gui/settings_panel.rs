//! Settings panel: edit sync rules (pattern → mode).

use eframe::egui;
use std::sync::mpsc::Sender;
use std::sync::{Arc, RwLock};

use crate::config::{SyncConfig, SyncMode, SyncRule};
use crate::engine::GuiCommand;

/// Mutable state for the settings editor within the GUI frame.
pub struct SettingsState {
    pub new_pattern: String,
    pub new_mode: SyncMode,
    pub editing: bool,
}

impl Default for SettingsState {
    fn default() -> Self {
        SettingsState {
            new_pattern: String::new(),
            new_mode: SyncMode::Reference,
            editing: false,
        }
    }
}

/// Render the settings panel.
pub fn settings_panel(
    ui: &mut egui::Ui,
    config: &Arc<RwLock<SyncConfig>>,
    commands_tx: &Sender<GuiCommand>,
    state: &mut SettingsState,
) {
    ui.heading("Settings");
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
