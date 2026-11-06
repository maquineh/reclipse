//! Original vector action icons for the Reclipse chrome.
//!
//! The marks are drawn from geometric primitives and are **not** copied from
//! Eclipse/WTP assets (which are EPL-2.0). They only reuse familiar IDE
//! affordances — a play triangle, a beetle, step arrows, pause/stop squares —
//! so the toolbar reads like a Java IDE while staying under the project's MIT
//! license.

use egui::{
    Align2, Color32, FontId, Painter, Pos2, Rect, Response, Sense, Shape, Stroke, Ui, Vec2,
};

use crate::theme;

/// Box used to normalize every glyph; `paint` scales it to the requested size.
const ICON_BOX: f32 = 16.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ActionIcon {
    Run,
    Debug,
    Compile,
    Build,
    Test,
    Launch,
    Continue,
    StepOver,
    StepInto,
    StepReturn,
    Pause,
    Stop,
    Restart,
    Reindex,
    Save,
    Search,
    References,
    Clear,
}

impl ActionIcon {
    #[cfg(test)]
    pub const ALL: [Self; 18] = [
        Self::Run,
        Self::Debug,
        Self::Compile,
        Self::Build,
        Self::Test,
        Self::Launch,
        Self::Continue,
        Self::StepOver,
        Self::StepInto,
        Self::StepReturn,
        Self::Pause,
        Self::Stop,
        Self::Restart,
        Self::Reindex,
        Self::Save,
        Self::Search,
        Self::References,
        Self::Clear,
    ];
}

/// Semantic tint for a glyph. The text label keeps the normal foreground
/// color; only the icon carries action semantics.
fn tint(icon: ActionIcon) -> Color32 {
    match icon {
        ActionIcon::Run | ActionIcon::Continue | ActionIcon::Test => theme::green(),
        ActionIcon::StepOver
        | ActionIcon::StepInto
        | ActionIcon::StepReturn
        | ActionIcon::Pause => theme::yellow(),
        ActionIcon::Stop | ActionIcon::Clear => theme::red(),
        ActionIcon::Compile
        | ActionIcon::Build
        | ActionIcon::Launch
        | ActionIcon::Debug
        | ActionIcon::Restart
        | ActionIcon::Reindex => theme::accent(),
        ActionIcon::Save | ActionIcon::Search | ActionIcon::References => theme::muted(),
    }
}

/// Full action button: semantic icon + label, with an optional accent fill.
pub fn action_button(ui: &mut Ui, icon: ActionIcon, label: &str, hint: &str) -> Response {
    button(ui, icon, Some(label), hint, false)
}

/// Accent action button used for the primary "run" action.
pub fn accent_button(ui: &mut Ui, icon: ActionIcon, label: &str, hint: &str) -> Response {
    button(ui, icon, Some(label), hint, true)
}

/// Compact icon-only button for debug-step clusters and test rows.
pub fn icon_button(ui: &mut Ui, icon: ActionIcon, hint: &str) -> Response {
    button(ui, icon, None, hint, false)
}

fn button(
    ui: &mut Ui,
    icon: ActionIcon,
    label: Option<&str>,
    hint: &str,
    accent: bool,
) -> Response {
    let font = FontId::proportional(12.0);
    let text_width = label
        .map(|label| {
            ui.painter()
                .layout_no_wrap(label.to_owned(), font.clone(), theme::text())
                .size()
                .x
        })
        .unwrap_or(0.0);
    let icon_size = if label.is_some() { 14.0 } else { 13.0 };
    let gap = if label.is_some() { 5.0 } else { 0.0 };
    let pad_x = if label.is_some() { 7.0 } else { 4.0 };
    let height = if label.is_some() { 22.0 } else { 20.0 };
    let width = pad_x * 2.0 + icon_size + gap + text_width;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), Sense::click());

    let painter = ui.painter();
    if accent {
        let fill = if response.hovered() {
            theme::accent().gamma_multiply(1.12)
        } else {
            theme::accent()
        };
        painter.rect_filled(rect, 4.0, fill);
    } else if response.hovered() {
        painter.rect_filled(rect, 4.0, theme::hover());
    }

    let foreground = if accent { theme::bg() } else { theme::text() };
    let icon_color = if accent { theme::bg() } else { tint(icon) };
    let icon_rect = Rect::from_center_size(
        Pos2::new(rect.left() + pad_x + icon_size * 0.5, rect.center().y),
        Vec2::splat(icon_size),
    );
    paint(painter, icon_rect, icon, icon_color);
    if let Some(label) = label {
        painter.text(
            Pos2::new(rect.left() + pad_x + icon_size + gap, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            font,
            foreground,
        );
    }

    response.on_hover_text(hint)
}

/// Paint one glyph into an arbitrary rectangle.
pub fn paint(painter: &Painter, rect: Rect, icon: ActionIcon, color: Color32) {
    let size = rect.width().min(rect.height());
    if size <= 0.0 || color.a() == 0 {
        return;
    }
    let pen = Pen {
        painter,
        center: rect.center(),
        scale: size / ICON_BOX,
        color,
    };
    match icon {
        ActionIcon::Run => {
            pen.polygon(&[(-5.5, -7.0), (-5.5, 7.0), (7.0, 0.0)], color);
        }
        ActionIcon::Continue => {
            pen.rect((-7.5, -7.0), (-4.0, 7.0), 1.0, true, 0.0);
            pen.polygon(&[(-2.5, -6.2), (-2.5, 6.2), (6.5, 0.0)], color);
        }
        ActionIcon::Debug => {
            // Beetle body, head, antennae and three pairs of legs.
            pen.rect((-4.2, -3.0), (4.2, 6.5), 4.0, false, 1.5);
            pen.circle((0.0, -5.2), 2.2, false, 1.5);
            pen.line((0.0, -2.8), (0.0, 5.8), 1.1);
            for (y, tilt) in [(-1.0, -2.0), (2.0, 0.0), (5.0, 2.0)] {
                pen.line((-4.0, y), (-7.2, y + tilt), 1.3);
                pen.line((4.0, y), (7.2, y + tilt), 1.3);
            }
            pen.line((-1.3, -6.8), (-3.0, -7.9), 1.2);
            pen.line((1.3, -6.8), (3.0, -7.9), 1.2);
        }
        ActionIcon::Compile => {
            // Mallet: thick vertical handle and a solid striking head.
            pen.line((0.0, 7.0), (0.0, -2.0), 2.8);
            pen.rect((-6.0, -6.5), (6.0, -2.0), 1.0, true, 0.0);
        }
        ActionIcon::Build => {
            // Clipboard: a build/checklist affordance.
            pen.rect((-5.0, -6.5), (5.0, 7.0), 1.5, false, 1.4);
            pen.rect((-2.8, -7.8), (2.8, -5.0), 1.0, true, 0.0);
            pen.line((-3.0, -2.5), (3.0, -2.5), 1.0);
            pen.line((-3.0, 0.0), (3.0, 0.0), 1.0);
            pen.line((-3.0, 2.5), (1.0, 2.5), 1.0);
            pen.tinted(theme::green())
                .polyline(&[(0.0, 4.8), (1.8, 6.4), (5.0, 3.0)], 1.6);
        }
        ActionIcon::Test => {
            // Erlenmeyer flask with liquid, bubbles and a passing check.
            pen.polyline(
                &[
                    (-2.2, -7.0),
                    (-2.2, -3.8),
                    (-6.0, 6.0),
                    (6.0, 6.0),
                    (2.2, -3.8),
                    (2.2, -7.0),
                ],
                1.4,
            );
            pen.line((-3.2, -7.0), (3.2, -7.0), 1.4);
            pen.polygon(
                &[(-4.6, 2.6), (-5.8, 5.7), (5.8, 5.7), (4.6, 2.6)],
                color.gamma_multiply(0.35),
            );
            pen.circle((-2.0, 4.3), 0.7, true, 0.0);
            pen.circle((1.2, 4.4), 0.55, true, 0.0);
            pen.tinted(theme::text())
                .polyline(&[(-2.8, 0.5), (-0.8, 2.5), (3.2, -1.5)], 1.8);
        }
        ActionIcon::Launch => {
            // Play + configuration sliders (launch configuration).
            pen.polygon(&[(-7.0, -6.0), (-7.0, 3.0), (-0.5, -1.5)], color);
            pen.line((1.5, -5.0), (7.0, -5.0), 1.0);
            pen.circle((4.8, -5.0), 1.2, true, 0.0);
            pen.line((1.5, 0.0), (7.0, 0.0), 1.0);
            pen.circle((3.3, 0.0), 1.2, true, 0.0);
            pen.line((1.5, 5.0), (7.0, 5.0), 1.0);
            pen.circle((5.6, 5.0), 1.2, true, 0.0);
        }
        ActionIcon::StepOver => {
            // Arc over the current line, arrow landing on the next line.
            pen.polyline(
                &[
                    (-6.0, -1.5),
                    (-6.0, -4.5),
                    (-2.5, -6.5),
                    (2.0, -6.5),
                    (5.0, -4.0),
                    (5.0, 1.5),
                ],
                1.5,
            );
            pen.polyline(&[(3.2, 2.0), (5.0, 4.2), (6.8, 2.0)], 1.5);
            pen.line((-6.5, 6.5), (6.5, 6.5), 1.2);
        }
        ActionIcon::StepInto => {
            // Arrow crossing the current line through a gap.
            pen.line((-7.0, 2.0), (-1.5, 2.0), 1.3);
            pen.line((1.5, 2.0), (7.0, 2.0), 1.3);
            pen.line((0.0, -7.0), (0.0, 4.0), 1.5);
            pen.polyline(&[(-2.2, 2.0), (0.0, 4.5), (2.2, 2.0)], 1.5);
        }
        ActionIcon::StepReturn => {
            // Arrow leaving the current line upward.
            pen.line((-7.0, 5.0), (7.0, 5.0), 1.3);
            pen.line((0.0, 7.0), (0.0, -4.0), 1.5);
            pen.polyline(&[(-2.2, -1.8), (0.0, -4.3), (2.2, -1.8)], 1.5);
        }
        ActionIcon::Pause => {
            pen.rect((-5.2, -6.0), (-1.6, 6.0), 0.8, true, 0.0);
            pen.rect((1.6, -6.0), (5.2, 6.0), 0.8, true, 0.0);
        }
        ActionIcon::Stop => {
            pen.rect((-6.0, -6.0), (6.0, 6.0), 1.5, true, 0.0);
        }
        ActionIcon::Restart | ActionIcon::Reindex => {
            pen.refresh();
        }
        ActionIcon::Save => {
            pen.rect((-6.0, -6.5), (6.0, 6.5), 1.0, false, 1.3);
            pen.rect((-3.5, -6.5), (3.0, -3.2), 0.5, true, 0.0);
            pen.rect((-4.0, 1.0), (4.0, 5.0), 0.5, false, 0.9);
        }
        ActionIcon::Search => {
            pen.circle((-1.5, -1.5), 4.4, false, 1.6);
            pen.line((1.8, 1.8), (6.5, 6.5), 2.0);
        }
        ActionIcon::References => {
            pen.circle((-2.5, -2.5), 3.4, false, 1.4);
            pen.line((0.2, 0.2), (3.5, 3.5), 1.7);
            pen.line((3.0, -6.0), (7.0, -6.0), 1.0);
            pen.line((3.0, -3.0), (7.0, -3.0), 1.0);
            pen.line((2.0, 6.0), (6.0, 6.0), 1.0);
        }
        ActionIcon::Clear => {
            pen.line((-5.0, -5.0), (5.0, 5.0), 1.7);
            pen.line((5.0, -5.0), (-5.0, 5.0), 1.7);
        }
    }
}

#[derive(Clone, Copy)]
struct Pen<'a> {
    painter: &'a Painter,
    center: Pos2,
    scale: f32,
    color: Color32,
}

impl<'a> Pen<'a> {
    fn tinted(&self, color: Color32) -> Self {
        Self { color, ..*self }
    }

    fn p(&self, x: f32, y: f32) -> Pos2 {
        self.center + Vec2::new(x, y) * self.scale
    }

    fn stroke(&self, width: f32) -> Stroke {
        Stroke::new((width * self.scale).max(1.0), self.color)
    }

    fn line(&self, a: (f32, f32), b: (f32, f32), width: f32) {
        self.painter
            .line_segment([self.p(a.0, a.1), self.p(b.0, b.1)], self.stroke(width));
    }

    fn polyline(&self, points: &[(f32, f32)], width: f32) {
        let points = points.iter().map(|&(x, y)| self.p(x, y)).collect();
        self.painter.add(Shape::line(points, self.stroke(width)));
    }

    fn polygon(&self, points: &[(f32, f32)], fill: Color32) {
        let points = points.iter().map(|&(x, y)| self.p(x, y)).collect();
        self.painter
            .add(Shape::convex_polygon(points, fill, Stroke::NONE));
    }

    fn circle(&self, center: (f32, f32), radius: f32, filled: bool, width: f32) {
        let center = self.p(center.0, center.1);
        let radius = radius * self.scale;
        if filled {
            self.painter.circle_filled(center, radius, self.color);
        } else {
            self.painter
                .circle_stroke(center, radius, self.stroke(width));
        }
    }

    fn rect(&self, min: (f32, f32), max: (f32, f32), radius: f32, filled: bool, width: f32) {
        let rect = Rect::from_min_max(self.p(min.0, min.1), self.p(max.0, max.1));
        let radius = radius * self.scale;
        if filled {
            self.painter.rect_filled(rect, radius, self.color);
        } else {
            self.painter
                .rect_stroke(rect, radius, self.stroke(width), egui::StrokeKind::Inside);
        }
    }

    fn refresh(&self) {
        let radius = 5.2;
        let start = -0.7_f32;
        let sweep = std::f32::consts::TAU * 0.82;
        let steps = 28;
        let mut points = Vec::with_capacity(steps + 1);
        for index in 0..=steps {
            let angle = start + sweep * (index as f32 / steps as f32);
            points.push(self.p(angle.cos() * radius, angle.sin() * radius));
        }
        let end_angle = start + sweep;
        let end = self.p(end_angle.cos() * radius, end_angle.sin() * radius);
        let tangent = Vec2::new(-end_angle.sin(), end_angle.cos());
        let normal = Vec2::new(end_angle.cos(), end_angle.sin());
        let tip = end + tangent * 2.5 * self.scale;
        let left = end - tangent * 0.2 * self.scale + normal * 2.0 * self.scale;
        let right = end - tangent * 0.2 * self.scale - normal * 2.0 * self.scale;
        self.painter.add(Shape::convex_polygon(
            vec![tip, left, right],
            self.color,
            Stroke::NONE,
        ));
        self.painter.add(Shape::line(points, self.stroke(1.4)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_icon_has_an_opaque_tint() {
        for icon in ActionIcon::ALL {
            assert_eq!(tint(icon).a(), 255, "{icon:?}");
        }
    }

    #[test]
    fn action_icons_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for icon in ActionIcon::ALL {
            assert!(seen.insert(icon), "duplicate {icon:?}");
        }
        assert_eq!(seen.len(), 18);
    }
}
