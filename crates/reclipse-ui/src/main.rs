//! Reclipse — a Rust-native Java IDE.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod build;
mod buildpath;
mod editor_view;
mod icons;
mod jdtls;
mod keymap;
mod palette;
mod recovery;
mod search;
mod session;
mod theme;
mod tree;

use std::path::PathBuf;

fn main() -> eframe::Result {
    let initial = std::env::args().nth(1).map(PathBuf::from);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([960.0, 600.0])
            .with_title("Reclipse — Rust-native Java IDE"),
        ..Default::default()
    };

    eframe::run_native(
        "Reclipse",
        options,
        Box::new(move |cc| Ok(Box::new(app::ReclipseApp::new(cc, initial)))),
    )
}
