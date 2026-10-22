mod app;
mod browse;
mod i18n;
mod list;
mod model;
mod move_dialog;
mod paths;
mod selection;
mod theme;
mod thumbs;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([900.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Photo Copy",
        options,
        Box::new(|cc| Ok(Box::new(app::PhotoCopyApp::new(cc)))),
    )
}
