//! Peers panel: list of connected peers with status indicators.

use eframe::egui;

use crate::net::peers::PeerInfo;

/// Render the peers panel.
pub fn peers_panel(ui: &mut egui::Ui, peers: &[PeerInfo]) {
    ui.heading("Peers");
    ui.separator();

    if peers.is_empty() {
        ui.label("No peers connected");
    } else {
        for peer in peers {
            ui.horizontal(|ui| {
                ui.colored_label(egui::Color32::from_rgb(0, 200, 0), "[*]");
                ui.label(&peer.remote_id);
            });
        }
    }

    ui.separator();
    ui.label(format!("Connected: {}", peers.len()));
}
