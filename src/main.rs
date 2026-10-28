//! Colonnade: a cross-platform database manager built with GPUI.

// Release builds on Windows must not spawn a console window next to the app.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    gpui_kit::application()
        .with_assets(colonnade::assets::AppAssets)
        .run(colonnade::app::run);
}
