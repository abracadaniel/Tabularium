//! Tabularium desktop application.
#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

mod admin_views;
mod app;
mod connection_dialog;
mod console;
mod copy_table;
mod ctx;
mod datagen_dialog;
mod dump_dialog;
mod editor;
mod er_diagram;
mod explorer;
mod export_dialog;
mod find_in_db;
mod goto;
mod grid;
mod history_view;
mod import_dialog;
mod plan;
mod runtime;
mod schema_compare;
mod secrets;
mod settings;
mod settings_dialog;
mod table_data;
mod table_designer;
mod text_view;
mod theme;

fn main() -> eframe::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,sqlx=warn,tiberius=warn")).init();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .thread_name("tabularium-db")
        .build()
        .expect("failed to start async runtime");

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Tabularium")
            .with_app_id("dev.tabularium.Tabularium")
            .with_inner_size([1400.0, 900.0])
            .with_min_inner_size([800.0, 500.0])
            .with_icon(app_icon()),
        persist_window: true,
        ..Default::default()
    };
    eframe::run_native("Tabularium", options, Box::new(move |cc| Ok(Box::new(app::TabulariumApp::new(cc, rt)))))
}

/// A small procedurally drawn icon: a table grid on a rounded blue square.
fn app_icon() -> egui::IconData {
    const N: usize = 64;
    let mut rgba = vec![0u8; N * N * 4];
    for y in 0..N {
        for x in 0..N {
            let i = (y * N + x) * 4;
            let (fx, fy) = (x as f32, y as f32);
            let r = 10.0;
            let cx = fx.clamp(r, N as f32 - 1.0 - r);
            let cy = fy.clamp(r, N as f32 - 1.0 - r);
            let inside = (fx - cx).powi(2) + (fy - cy).powi(2) <= r * r;
            if !inside {
                continue;
            }
            let grid = (x > 12 && x < 52 && y > 14 && y < 50) && (x % 13 == 0 || y % 12 == 2 || y < 21);
            let (cr, cg, cb) = if grid { (240, 244, 255) } else { (44, 92, 196) };
            rgba[i..i + 4].copy_from_slice(&[cr, cg, cb, 255]);
        }
    }
    egui::IconData { rgba, width: N as u32, height: N as u32 }
}
