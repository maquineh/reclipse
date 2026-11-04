//! Reclipse visual identity: a calm, high-contrast dark theme tuned for code.

use egui::{Color32, Context, FontId, Stroke, TextStyle, Vec2};

pub const BG: Color32 = Color32::from_rgb(0x15, 0x18, 0x1d);
pub const PANEL: Color32 = Color32::from_rgb(0x1c, 0x20, 0x27);
pub const PANEL_ALT: Color32 = Color32::from_rgb(0x22, 0x27, 0x30);
pub const HOVER: Color32 = Color32::from_rgb(0x2b, 0x32, 0x3e);
pub const EDITOR_BG: Color32 = Color32::from_rgb(0x17, 0x1a, 0x20);
pub const TEXT: Color32 = Color32::from_rgb(0xd7, 0xdd, 0xe8);
pub const MUTED: Color32 = Color32::from_rgb(0x8b, 0x95, 0xa8);
pub const ACCENT: Color32 = Color32::from_rgb(0x5b, 0x9d, 0xf5);
pub const ACCENT_DIM: Color32 = Color32::from_rgb(0x2c, 0x46, 0x69);
pub const BORDER: Color32 = Color32::from_rgb(0x2d, 0x34, 0x40);
pub const GREEN: Color32 = Color32::from_rgb(0x7d, 0xc9, 0x92);
pub const YELLOW: Color32 = Color32::from_rgb(0xdc, 0xd2, 0x8a);
pub const RED: Color32 = Color32::from_rgb(0xf0, 0x7e, 0x7e);

pub const CLR_KEYWORD: Color32 = Color32::from_rgb(0x56, 0x9c, 0xd6);
pub const CLR_TYPE: Color32 = Color32::from_rgb(0x4e, 0xc9, 0xb0);
pub const CLR_STRING: Color32 = Color32::from_rgb(0xce, 0x91, 0x78);
pub const CLR_COMMENT: Color32 = Color32::from_rgb(0x6a, 0x99, 0x55);
pub const CLR_NUMBER: Color32 = Color32::from_rgb(0xb5, 0xce, 0xa8);
pub const CLR_ANNOTATION: Color32 = Color32::from_rgb(0xdc, 0xdc, 0xaa);
pub const CLR_LITERAL: Color32 = Color32::from_rgb(0x56, 0x9c, 0xd6);

pub fn configure(ctx: &Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = PANEL;
    visuals.window_fill = PANEL;
    visuals.extreme_bg_color = EDITOR_BG;
    visuals.faint_bg_color = PANEL_ALT;
    visuals.override_text_color = Some(TEXT);
    visuals.selection.bg_fill = ACCENT_DIM;
    visuals.selection.stroke = Stroke::new(1.0_f32, ACCENT);
    visuals.window_stroke = Stroke::new(1.0_f32, BORDER);
    visuals.widgets.noninteractive.bg_fill = PANEL;
    visuals.widgets.noninteractive.weak_bg_fill = PANEL;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    visuals.widgets.inactive.bg_fill = PANEL_ALT;
    visuals.widgets.inactive.weak_bg_fill = PANEL_ALT;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    visuals.widgets.hovered.bg_fill = HOVER;
    visuals.widgets.hovered.weak_bg_fill = HOVER;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, ACCENT_DIM);
    visuals.widgets.active.bg_fill = ACCENT_DIM;
    visuals.widgets.active.weak_bg_fill = ACCENT_DIM;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, ACCENT);

    let mut style = (*ctx.style()).clone();
    // Apply the palette through the style as well: `Window` and widgets use
    // `style.visuals`, and setting only the context visuals can leave windows
    // on the system (light) theme.
    style.visuals = visuals;
    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.button_padding = Vec2::new(8.0, 4.0);
    style.spacing.menu_margin = egui::Margin::same(6);
    style
        .text_styles
        .insert(TextStyle::Body, FontId::proportional(14.0));
    style
        .text_styles
        .insert(TextStyle::Button, FontId::proportional(14.0));
    style
        .text_styles
        .insert(TextStyle::Small, FontId::proportional(12.0));
    style
        .text_styles
        .insert(TextStyle::Heading, FontId::proportional(20.0));
    style
        .text_styles
        .insert(TextStyle::Monospace, FontId::monospace(14.0));

    // Reclipse is dark-only for now. Pin the theme and apply the same style to
    // both theme slots so a Windows theme switch cannot turn windows light.
    ctx.set_theme(egui::Theme::Dark);
    ctx.set_style_of(egui::Theme::Dark, style.clone());
    ctx.set_style_of(egui::Theme::Light, style);
}
