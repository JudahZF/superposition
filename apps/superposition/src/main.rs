//! Desktop application composition root with an egui live-rack shell.
//!
//! This binary intentionally depends only on application-safe boundary crates. VST3 SDK
//! interaction belongs in the isolated worker and scanner helper processes.

mod ui;

use std::path::PathBuf;

use eframe::egui;
use sp_session::SessionController;
use sp_supervisor::RackSupervisor;

use ui::LiveRackApp;

fn main() -> eframe::Result<()> {
    let session_root = default_session_root();
    let controller = SessionController::open(&session_root)
        .unwrap_or_else(|_| SessionController::empty(&session_root));
    let recovery_offered = controller.recovery_offered();
    let _supervisor = RackSupervisor::new();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 720.0])
            .with_min_inner_size([960.0, 640.0])
            .with_title("Superposition"),
        ..Default::default()
    };

    eframe::run_native(
        "Superposition",
        native_options,
        Box::new(move |cc| {
            ui::install_fonts(&cc.egui_ctx);
            Ok(Box::new(LiveRackApp::new(controller, recovery_offered)))
        }),
    )
}

fn default_session_root() -> PathBuf {
    std::env::var_os("SUPERPOSITION_SESSION").map_or_else(
        || dirs_fallback().join("Library/Application Support/Superposition/Default.superposition"),
        PathBuf::from,
    )
}

fn dirs_fallback() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}
