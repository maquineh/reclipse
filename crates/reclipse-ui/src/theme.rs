//! Reclipse visual identity: RECLIPSE Studio design system.
//!
//! The default theme is **Rust Oxide Industrial** (`#111418` surfaces,
//! `#E05A2B` primary, `#F59E0B` corona). Two alternate dark themes
//! (**Eclipse Dark Cósmico** and **Cyber Solar Eclipse**) can be selected
//! in the `Exibir → Tema` menu; the choice is persisted in the session.

use egui::{Color32, Context, FontId, Stroke, TextStyle, Vec2};
use std::sync::atomic::{AtomicUsize, Ordering};

static ACTIVE_THEME: AtomicUsize = AtomicUsize::new(0);

/// `RECLIPSE Studio v1.0` — switchable theme set (see `docs/DESIGN_SYSTEM.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThemeKind {
    RustOxide,
    EclipseCosmic,
    CyberSolar,
}

impl ThemeKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::RustOxide => "Rust Oxide Industrial (padrão)",
            Self::EclipseCosmic => "Eclipse Dark Cósmico",
            Self::CyberSolar => "Cyber Solar Eclipse",
        }
    }

    pub fn short_label(self) -> &'static str {
        match self {
            Self::RustOxide => "Rust Oxide",
            Self::EclipseCosmic => "Eclipse Cosmic",
            Self::CyberSolar => "Cyber Solar",
        }
    }

    fn slot(self) -> usize {
        match self {
            Self::RustOxide => 0,
            Self::EclipseCosmic => 1,
            Self::CyberSolar => 2,
        }
    }

    #[allow(dead_code)]
    fn from_slot(slot: usize) -> Self {
        match slot % 3 {
            1 => Self::EclipseCosmic,
            2 => Self::CyberSolar,
            _ => Self::RustOxide,
        }
    }

    pub fn from_name(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "eclipse" | "eclipse-cosmic" | "cosmic" => Self::EclipseCosmic,
            "cyber" | "cyber-solar" | "solar" => Self::CyberSolar,
            _ => Self::RustOxide,
        }
    }

    pub fn config_name(self) -> &'static str {
        match self {
            Self::RustOxide => "rust-oxide",
            Self::EclipseCosmic => "eclipse-cosmic",
            Self::CyberSolar => "cyber-solar",
        }
    }
}

/// A complete dark palette. Field semantics follow the design system:
/// `bg` canvas, `panel` surface-container, `panel_alt` container-high,
/// `hover` hover fill, `accent` primary, `accent_dim` active fill.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub bg: Color32,
    pub panel: Color32,
    pub panel_alt: Color32,
    pub hover: Color32,
    pub editor_bg: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub accent: Color32,
    pub accent_dim: Color32,
    pub border: Color32,
    pub green: Color32,
    pub yellow: Color32,
    pub red: Color32,
    pub clr_keyword: Color32,
    pub clr_type: Color32,
    pub clr_string: Color32,
    pub clr_comment: Color32,
    pub clr_number: Color32,
    pub clr_annotation: Color32,
    pub clr_literal: Color32,
}

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb(
        ((hex >> 16) & 0xFF) as u8,
        ((hex >> 8) & 0xFF) as u8,
        (hex & 0xFF) as u8,
    )
}

/// Raster mark of the Reclipse emblem (corona ring + crab shell disc + `R`).
/// `accent` and `corona` receive the theme's primary/corona colors.
pub fn emblem_color(kind: ThemeKind, ring: bool) -> Color32 {
    let (primary, corona, _dark_primary) = match kind {
        ThemeKind::RustOxide => (rgb(0xE05A2B), rgb(0xF59E0B), rgb(0xFF7A45)),
        ThemeKind::EclipseCosmic => (rgb(0x8B5CF6), rgb(0x06B6D4), rgb(0xC084FC)),
        ThemeKind::CyberSolar => (rgb(0xF59E0B), rgb(0x06B6D4), rgb(0x10B981)),
    };
    if ring {
        corona
    } else {
        primary
    }
}

const PALETTES: [Palette; 3] = [
    // Rust Oxide Industrial (default).
    Palette {
        bg: rgb(0x11_14_18),
        panel: rgb(0x18_1D_24),
        panel_alt: rgb(0x222832),
        hover: rgb(0x2A_31_3D),
        editor_bg: rgb(0x0D_10_15),
        text: rgb(0xE2_E8_F0),
        muted: rgb(0x8B_95_A8),
        accent: rgb(0xE0_5A_2B),
        accent_dim: rgb(0x3D_2A_1F),
        border: rgb(0x37_41_51),
        green: rgb(0x22_C5_5E),
        yellow: rgb(0xF5_9E_0B),
        red: rgb(0xEF_44_44),
        clr_keyword: rgb(0xE0_5A_2B),
        clr_type: rgb(0x38_BD_F8),
        clr_string: rgb(0xF5_9E_0B),
        clr_comment: rgb(0x64_74_8B),
        clr_number: rgb(0xEC_48_99),
        clr_annotation: rgb(0xA8_55_F7),
        clr_literal: rgb(0x38_BD_F8),
    },
    // Eclipse Dark Cosmico.
    Palette {
        bg: rgb(0x0D_11_17),
        panel: rgb(0x16_1B_22),
        panel_alt: rgb(0x21_26_33),
        hover: rgb(0x2C_33_3D),
        editor_bg: rgb(0x0D_11_17),
        text: rgb(0xF0_F6_FC),
        muted: rgb(0x8B_95_A8),
        accent: rgb(0x8B_5C_F6),
        accent_dim: rgb(0x2E_24_4D),
        border: rgb(0x30_36_3D),
        green: rgb(0x3F_B9_5C),
        yellow: rgb(0xF5_9E_0B),
        red: rgb(0xF8_51_49),
        clr_keyword: rgb(0xC0_84_FC),
        clr_type: rgb(0x38_BD_F8),
        clr_string: rgb(0xA5_D6_FF),
        clr_comment: rgb(0x8B_94_9C),
        clr_number: rgb(0xFF_9B_CE),
        clr_annotation: rgb(0xA8_55_F7),
        clr_literal: rgb(0x38_BD_F8),
    },
    // Cyber Solar Eclipse.
    Palette {
        bg: rgb(0x09_0B_0E),
        panel: rgb(0x0F_13_1A),
        panel_alt: rgb(0x17_1D_27),
        hover: rgb(0x22_29_37),
        editor_bg: rgb(0x07_09_0D),
        text: rgb(0xE5_E7_EB),
        muted: rgb(0x8B_95_A8),
        accent: rgb(0xF5_9E_0B),
        accent_dim: rgb(0x3A_2E_0D),
        border: rgb(0x21_26_2D),
        green: rgb(0x10_B9_81),
        yellow: rgb(0xF5_9E_0B),
        red: rgb(0xEF_44_44),
        clr_keyword: rgb(0xF5_9E_0B),
        clr_type: rgb(0x06_B6_D4),
        clr_string: rgb(0x10_B9_81),
        clr_comment: rgb(0x64_74_8B),
        clr_number: rgb(0xEC_48_99),
        clr_annotation: rgb(0x38_BD_F8),
        clr_literal: rgb(0x06_B6_D4),
    },
];

pub fn configure(ctx: &Context, kind: ThemeKind) {
    ACTIVE_THEME.store(kind.slot(), Ordering::Relaxed);
    let p = PALETTES[kind.slot()];
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = p.panel;
    visuals.window_fill = p.panel;
    visuals.extreme_bg_color = p.editor_bg;
    visuals.faint_bg_color = p.panel_alt;
    visuals.override_text_color = Some(p.text);
    visuals.selection.bg_fill = p.accent_dim;
    visuals.selection.stroke = Stroke::new(1.0_f32, p.accent);
    visuals.window_stroke = Stroke::new(1.0_f32, p.border);
    visuals.widgets.noninteractive.bg_fill = p.panel;
    visuals.widgets.noninteractive.weak_bg_fill = p.panel;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, p.border);
    visuals.widgets.inactive.bg_fill = p.panel_alt;
    visuals.widgets.inactive.weak_bg_fill = p.panel_alt;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, p.border);
    visuals.widgets.hovered.bg_fill = p.hover;
    visuals.widgets.hovered.weak_bg_fill = p.hover;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, p.accent_dim);
    visuals.widgets.active.bg_fill = p.accent_dim;
    visuals.widgets.active.weak_bg_fill = p.accent_dim;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, p.accent);

    let mut style = (*ctx.style()).clone();
    style.visuals = visuals;
    // Design-system spacing: 4px rhythm, compact tool density.
    style.spacing.item_spacing = Vec2::new(8.0, 4.0);
    style.spacing.button_padding = Vec2::new(8.0, 4.0);
    style.spacing.menu_margin = egui::Margin::same(4);
    style.spacing.indent = 12.0;
    // Design-system typography (egui built-ins are rendered; Cal Sans /
    // Commissioner fall back to the proportional face, Source Code Pro maps
    // to the monospace face).
    style
        .text_styles
        .insert(TextStyle::Body, FontId::proportional(13.0));
    style
        .text_styles
        .insert(TextStyle::Button, FontId::proportional(13.0));
    style
        .text_styles
        .insert(TextStyle::Small, FontId::proportional(11.0));
    style
        .text_styles
        .insert(TextStyle::Heading, FontId::proportional(20.0));
    style
        .text_styles
        .insert(TextStyle::Monospace, FontId::monospace(13.0));

    // Reclipse is dark-only. Pin the theme and apply the same style to both
    // slots so a Windows theme switch cannot turn windows light.
    ctx.set_theme(egui::Theme::Dark);
    ctx.set_style_of(egui::Theme::Dark, style.clone());
    ctx.set_style_of(egui::Theme::Light, style);
}

/// Current palette for context-free consumers (colors only).
#[allow(dead_code)]
pub fn palette_for(kind: ThemeKind) -> Palette {
    PALETTES[kind.slot()]
}

fn active() -> Palette {
    PALETTES[ACTIVE_THEME.load(Ordering::Relaxed) % PALETTES.len()]
}

macro_rules! color_accessor {
    ($($name:ident => $field:ident),* $(,)?) => {
        $(
            pub fn $name() -> Color32 {
                active().$field
            }
        )*
    };
}

color_accessor! {
    bg => bg,
    panel => panel,
    panel_alt => panel_alt,
    hover => hover,
    editor_bg => editor_bg,
    text => text,
    muted => muted,
    accent => accent,
    accent_dim => accent_dim,
    border => border,
    green => green,
    yellow => yellow,
    red => red,
    clr_keyword => clr_keyword,
    clr_type => clr_type,
    clr_string => clr_string,
    clr_comment => clr_comment,
    clr_number => clr_number,
    clr_annotation => clr_annotation,
    clr_literal => clr_literal,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palettes_are_distinct_dark_surfaces() {
        let oxid = palette_for(ThemeKind::RustOxide);
        let cosmic = palette_for(ThemeKind::EclipseCosmic);
        let solar = palette_for(ThemeKind::CyberSolar);
        assert_ne!(oxid.bg, cosmic.bg);
        assert_ne!(cosmic.bg, solar.bg);
        for palette in [oxid, cosmic, solar] {
            // Dark canvases: luminance well below chrome surfaces.
            let lum = |c: Color32| c.r() as u32 + c.g() as u32 + c.b() as u32;
            assert!(lum(palette.bg) < lum(palette.panel));
            assert!(lum(palette.bg) < lum(palette.text));
        }
    }

    #[test]
    fn theme_slots_round_trip() {
        assert_eq!(ThemeKind::from_slot(0), ThemeKind::RustOxide);
        assert_eq!(ThemeKind::from_slot(1), ThemeKind::EclipseCosmic);
        assert_eq!(ThemeKind::from_slot(2), ThemeKind::CyberSolar);
        assert_eq!(ThemeKind::from_slot(3), ThemeKind::RustOxide);
    }

    #[test]
    fn accessors_follow_the_active_theme() {
        let ctx = Context::default();
        configure(&ctx, ThemeKind::EclipseCosmic);
        assert_eq!(accent(), PALETTES[1].accent);
        assert_eq!(clr_keyword(), PALETTES[1].clr_keyword);
        assert_eq!(panel(), PALETTES[1].panel);
        configure(&ctx, ThemeKind::CyberSolar);
        assert_eq!(accent(), PALETTES[2].accent);
        assert_eq!(clr_keyword(), PALETTES[2].clr_keyword);
        configure(&ctx, ThemeKind::RustOxide);
        assert_eq!(accent(), PALETTES[0].accent);
    }
}
