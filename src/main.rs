// Release builds are GUI-only (no console window on Windows).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;

use std::path::PathBuf;

use eframe::egui;

/// Usage: huge-file-reader [FILE [SEARCH TEXT...]]
fn main() -> eframe::Result {
    let mut args = std::env::args_os().skip(1);
    let file = args.next().map(PathBuf::from);
    // Several search texts go into the search box one per line.
    let texts: Vec<String> = args.map(|q| q.to_string_lossy().into_owned()).collect();
    let query = (!texts.is_empty()).then(|| texts.join("\n"));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Huge File Reader")
            .with_inner_size([1280.0, 860.0])
            .with_min_inner_size([720.0, 480.0])
            .with_drag_and_drop(true)
            .with_icon(icon()),
        ..Default::default()
    };
    eframe::run_native(
        "Huge File Reader",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, file, query, Some(hfr::link::port()))))),
    )
}

/// A small magnifying-glass icon, drawn procedurally.
fn icon() -> egui::IconData {
    const S: usize = 64;
    let mut rgba = vec![0u8; S * S * 4];
    let (cx, cy, r_out, r_in) = (26.0f32, 26.0f32, 20.0f32, 14.0f32);
    for y in 0..S {
        for x in 0..S {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let d = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();
            // Distance to the handle segment from (40,40) to (58,58).
            let t = (((fx - 40.0) + (fy - 40.0)) / 2.0).clamp(0.0, 18.0);
            let dh = ((fx - 40.0 - t).powi(2) + (fy - 40.0 - t).powi(2)).sqrt();
            let px = &mut rgba[(y * S + x) * 4..][..4];
            if d <= r_out && d >= r_in || dh <= 5.0 {
                px.copy_from_slice(&[40, 110, 220, 255]);
            } else if d < r_in {
                // Lens with three "text lines".
                let line = (fy as i32 - 17) % 7 < 3 && (fy as i32) > 16 && (fy as i32) < 37 && fx > 16.0 && fx < 36.0;
                px.copy_from_slice(if line { &[120, 160, 230, 255] } else { &[235, 242, 255, 255] });
            }
        }
    }
    egui::IconData { rgba, width: S as u32, height: S as u32 }
}
