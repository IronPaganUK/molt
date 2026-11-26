//! Look and feel: fonts, colours, spacing, and a few shared widgets.
//!
//! Colour has meaning here: blue is the everyday "go" colour, orange means
//! the archive will be consumed (destructive, no undo), green means bytes came
//! back, red means something failed.

use eframe::egui::{
    self, Color32, FontData, FontDefinitions, FontFamily, FontId, Margin, RichText, Rounding,
    Stroke, TextStyle, Vec2,
};

pub const ACCENT: Color32 = Color32::from_rgb(0x3d, 0x84, 0xf5);
pub const MOLT: Color32 = Color32::from_rgb(0xdd, 0x6b, 0x2c);
pub const OK: Color32 = Color32::from_rgb(0x5c, 0xc4, 0x7a);
pub const INFO: Color32 = Color32::from_rgb(0x7d, 0xb2, 0xf2);
pub const WARN: Color32 = Color32::from_rgb(0xe8, 0xb5, 0x4a);
pub const ERR: Color32 = Color32::from_rgb(0xee, 0x6b, 0x6b);

pub const BG: Color32 = Color32::from_rgb(0x1b, 0x1c, 0x20);
pub const SURFACE: Color32 = Color32::from_rgb(0x23, 0x25, 0x2a);
pub const SUNKEN: Color32 = Color32::from_rgb(0x15, 0x16, 0x19);
pub const BORDER: Color32 = Color32::from_rgb(0x33, 0x36, 0x3d);
pub const TEXT: Color32 = Color32::from_rgb(0xe3, 0xe5, 0xe9);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x95, 0x9a, 0xa4);

const SEMIBOLD: &str = "semibold";

/// Semibold UI font (Segoe UI Semibold on Windows; regular elsewhere).
pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(SEMIBOLD.into()))
}

pub fn dim(text: impl Into<String>) -> RichText {
    RichText::new(text).color(TEXT_DIM)
}

pub fn install(ctx: &egui::Context) {
    install_fonts(ctx);

    let mut style = (*ctx.style()).clone();
    style.text_styles = [
        (TextStyle::Heading, semibold(19.0)),
        (TextStyle::Body, FontId::proportional(14.0)),
        (TextStyle::Button, FontId::proportional(14.0)),
        (TextStyle::Small, FontId::proportional(12.0)),
        (TextStyle::Monospace, FontId::monospace(13.0)),
    ]
    .into();

    let s = &mut style.spacing;
    s.item_spacing = Vec2::new(8.0, 6.0);
    s.button_padding = Vec2::new(12.0, 5.0);
    s.interact_size.y = 26.0;
    s.window_margin = Margin::same(18.0);
    s.menu_margin = Margin::same(6.0);
    s.icon_width = 16.0;
    s.icon_width_inner = 9.0;

    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = SURFACE;
    v.extreme_bg_color = SUNKEN;
    v.faint_bg_color = Color32::from_rgb(0x20, 0x22, 0x26); // table stripes
    v.window_rounding = Rounding::same(10.0);
    v.menu_rounding = Rounding::same(8.0);
    v.window_stroke = Stroke::new(1.0_f32, BORDER);
    v.selection.bg_fill = ACCENT;
    v.selection.stroke = Stroke::new(1.5_f32, Color32::WHITE);
    v.hyperlink_color = INFO;
    v.error_fg_color = ERR;
    v.warn_fg_color = WARN;

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = SURFACE;
    w.noninteractive.weak_bg_fill = SURFACE;
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    w.noninteractive.rounding = Rounding::same(6.0);

    let fill = Color32::from_rgb(0x30, 0x33, 0x3a);
    let hover = Color32::from_rgb(0x3a, 0x3e, 0x46);
    let press = Color32::from_rgb(0x45, 0x4a, 0x53);
    for (state, bg) in [
        (&mut w.inactive, fill),
        (&mut w.hovered, hover),
        (&mut w.active, press),
        (&mut w.open, hover),
    ] {
        state.bg_fill = bg;
        state.weak_bg_fill = bg;
        state.bg_stroke = Stroke::NONE;
        state.fg_stroke = Stroke::new(1.0_f32, TEXT);
        state.rounding = Rounding::same(6.0);
        state.expansion = 0.0;
    }
    w.hovered.bg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(0x55, 0x5a, 0x64));
    w.hovered.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
    w.active.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);

    style.visuals = v;
    ctx.set_style(style);
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let mut semibold_face: Option<&str> = None;

    // Use the system UI font on Windows; egui's bundled fonts stay behind it
    // as fallbacks for symbols and emoji.
    #[cfg(windows)]
    {
        let dir = std::env::var_os("WINDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| "C:\\Windows".into())
            .join("Fonts");
        let load = |f: &str| std::fs::read(dir.join(f)).ok();
        if let Some(bytes) = load("segoeui.ttf") {
            fonts.font_data.insert("segoe".into(), FontData::from_owned(bytes));
            fonts
                .families
                .entry(FontFamily::Proportional)
                .or_default()
                .insert(0, "segoe".into());
        }
        if let Some(bytes) = load("seguisb.ttf") {
            fonts.font_data.insert("segoe-sb".into(), FontData::from_owned(bytes));
            semibold_face = Some("segoe-sb");
        }
        if let Some(bytes) = load("seguisym.ttf") {
            fonts.font_data.insert("segoe-sym".into(), FontData::from_owned(bytes));
            fonts
                .families
                .entry(FontFamily::Proportional)
                .or_default()
                .push("segoe-sym".into());
        }
    }

    let mut family: Vec<String> = semibold_face.map(String::from).into_iter().collect();
    family.extend(fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default());
    fonts.families.insert(FontFamily::Name(SEMIBOLD.into()), family);
    ctx.set_fonts(fonts);
}

/// A filled call-to-action button in `color` with white text.
pub fn primary(text: impl Into<String>, color: Color32) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text).font(semibold(14.0)).color(Color32::WHITE))
        .fill(color)
        .min_size(Vec2::new(96.0, 30.0))
}

/// A plain secondary button sized to sit next to [`primary`].
pub fn secondary(text: impl Into<String>) -> egui::Button<'static> {
    egui::Button::new(text.into()).min_size(Vec2::new(84.0, 30.0))
}

/// Segmented switch between options (each: value, label, tooltip, colour
/// when selected). Returns true when the user changed the value.
pub fn segmented<T: PartialEq + Copy>(
    ui: &mut egui::Ui,
    value: &mut T,
    options: &[(T, &str, &str, Color32)],
    enabled: bool,
) -> bool {
    let mut changed = false;
    egui::Frame::none()
        .fill(SUNKEN)
        .rounding(8.0)
        .inner_margin(Margin::same(3.0))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            ui.horizontal(|ui| {
                for &(opt, label, tip, color) in options {
                    let selected = *value == opt;
                    let resp = if selected {
                        ui.add_enabled(
                            enabled,
                            egui::Button::new(RichText::new(label).color(Color32::WHITE))
                                .fill(color)
                                .min_size(Vec2::new(0.0, 26.0)),
                        )
                    } else {
                        ui.add_enabled(enabled, egui::SelectableLabel::new(false, label))
                    };
                    if resp.on_hover_text(tip).clicked() && !selected {
                        *value = opt;
                        changed = true;
                    }
                }
            });
        });
    changed
}

/// Darken everything behind a dialog and swallow clicks on it, so the
/// dialog (drawn at `Order::Foreground`) is genuinely modal.
pub fn dim_background(ctx: &egui::Context) {
    let screen = ctx.screen_rect();
    egui::Area::new(egui::Id::new("molt-modal-dim"))
        .order(egui::Order::Middle)
        .fixed_pos(screen.min)
        .show(ctx, |ui| {
            ui.painter().rect_filled(screen, 0.0, Color32::from_black_alpha(150));
            ui.allocate_rect(screen, egui::Sense::click());
        });
}

/// Standard dialog window: no title bar (the dialog draws its own heading),
/// centred, above the dimmed background. Callers set the width with
/// `ui.set_width`.
pub fn dialog(id: &str) -> egui::Window<'static> {
    egui::Window::new(id.to_owned())
        .id(egui::Id::new(id.to_owned()))
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
}

/// Right-aligned dialog button row. Add the rightmost button first
/// (Windows order: the primary action sits to the left of Cancel).
pub fn button_row(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(14.0);
    ui.allocate_ui_with_layout(
        Vec2::new(ui.available_width(), 32.0),
        egui::Layout::right_to_left(egui::Align::Center),
        add,
    );
}
