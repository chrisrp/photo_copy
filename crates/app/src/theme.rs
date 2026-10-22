//! The app's single look ("Darkroom"): near-black and monochrome so the photos stand
//! out. Colour is used only for status: green = done, amber = work left, red = destructive.

use eframe::egui::{self, Color32, CornerRadius, Stroke};

pub const BG: Color32 = Color32::from_rgb(0x0e, 0x0e, 0x0f);
/// Raised surfaces: selected rows, the current step, the selection bar.
pub const PANEL: Color32 = Color32::from_rgb(0x18, 0x18, 0x1a);
pub const HOVER: Color32 = Color32::from_rgb(0x22, 0x22, 0x24);
pub const BORDER: Color32 = Color32::from_rgb(0x22, 0x22, 0x24);
/// Outline of buttons and other controls.
pub const CONTROL: Color32 = Color32::from_rgb(0x3a, 0x3a, 0x3c);
pub const TEXT: Color32 = Color32::from_rgb(0xed, 0xed, 0xed);
pub const STRONG: Color32 = Color32::WHITE;
/// Secondary text such as paths of files that can be removed.
pub const DIM: Color32 = Color32::from_rgb(0xa1, 0xa1, 0xa6);
pub const MUTED: Color32 = Color32::from_rgb(0x8e, 0x8e, 0x93);
pub const GREEN: Color32 = Color32::from_rgb(0x30, 0xd1, 0x58);
pub const AMBER: Color32 = Color32::from_rgb(0xff, 0xb3, 0x40);
pub const RED: Color32 = Color32::from_rgb(0xff, 0x69, 0x61);
pub const RED_BORDER: Color32 = Color32::from_rgb(0x5a, 0x2a, 0x2a);
pub const AMBER_BG: Color32 = Color32::from_rgb(0x1d, 0x16, 0x08);
pub const AMBER_BORDER: Color32 = Color32::from_rgb(0x3d, 0x2e, 0x10);
pub const AMBER_TEXT: Color32 = Color32::from_rgb(0xf3, 0xd3, 0x9a);

pub const RADIUS: u8 = 4;

/// Install the look. There is no light variant: the app always uses this one.
pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.set_visuals_of(egui::Theme::Dark, visuals());
    ctx.style_mut_of(egui::Theme::Dark, |style| {
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 5.0);
        style.spacing.interact_size.y = 24.0;
    });
}

fn visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    let radius = CornerRadius::same(RADIUS);
    v.panel_fill = BG;
    v.window_fill = PANEL;
    v.window_stroke = Stroke::new(1.0, CONTROL);
    v.window_corner_radius = CornerRadius::same(6);
    v.menu_corner_radius = radius;
    v.extreme_bg_color = Color32::from_rgb(0x08, 0x08, 0x09);
    v.faint_bg_color = PANEL;
    v.code_bg_color = PANEL;
    v.hyperlink_color = TEXT;
    v.warn_fg_color = AMBER;
    v.error_fg_color = RED;
    v.selection.bg_fill = Color32::from_rgb(0x2c, 0x2c, 0x2e);
    v.selection.stroke = Stroke::new(1.0, STRONG);

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = BG;
    w.noninteractive.weak_bg_fill = BG;
    w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);

    w.inactive.bg_fill = Color32::from_rgb(0x2c, 0x2c, 0x2e);
    w.inactive.weak_bg_fill = Color32::TRANSPARENT;
    w.inactive.bg_stroke = Stroke::new(1.0, CONTROL);
    w.inactive.fg_stroke = Stroke::new(1.0, TEXT);

    w.hovered.bg_fill = Color32::from_rgb(0x3a, 0x3a, 0x3c);
    w.hovered.weak_bg_fill = HOVER;
    w.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0x63, 0x63, 0x66));
    w.hovered.fg_stroke = Stroke::new(1.5, STRONG);

    w.active.bg_fill = Color32::from_rgb(0x48, 0x48, 0x4a);
    w.active.weak_bg_fill = Color32::from_rgb(0x2c, 0x2c, 0x2e);
    w.active.bg_stroke = Stroke::new(1.0, STRONG);
    w.active.fg_stroke = Stroke::new(2.0, STRONG);

    w.open = w.hovered;
    for widget in [
        &mut w.noninteractive,
        &mut w.inactive,
        &mut w.hovered,
        &mut w.active,
        &mut w.open,
    ] {
        widget.corner_radius = radius;
        widget.expansion = 0.0;
    }
    v
}

/// The one main action on screen: white with dark text.
pub fn primary(text: impl Into<String>) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text).color(BG).strong()).fill(TEXT)
}

/// An action that removes something (always followed by a confirmation).
pub fn danger(text: impl Into<String>) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text).color(RED).small())
        .stroke(Stroke::new(1.0, RED_BORDER))
}

/// Small coloured dot used for status next to a label.
pub fn dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 3.0, color);
}

/// Monospace text, used for paths.
pub fn mono(text: impl Into<String>, color: Color32) -> egui::RichText {
    egui::RichText::new(text).monospace().color(color)
}

/// Small uppercase label such as `DESTINATION`.
pub fn caps(text: &str) -> egui::RichText {
    egui::RichText::new(text.to_uppercase())
        .small()
        .color(MUTED)
}
