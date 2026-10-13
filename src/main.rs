//! Colonnade: a cross-platform database manager built with GPUI.

// Release builds on Windows must not spawn a console window next to the app.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;

fn main() {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(app::run);
}
