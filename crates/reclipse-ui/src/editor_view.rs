//! Virtualized code editor rendered directly with egui.
//!
//! Only visible lines are laid out and painted. The buffer is a rope and the
//! lexical state is updated incrementally after edits.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use egui::text::{LayoutJob, LayoutSection, TextFormat};
use egui::{Align, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Ui, Vec2};
use reclipse_editor::{tokenize_line, CursorState, Document, HighlightState, TokenKind, TokenSpan};

use crate::theme;

const TAB_SIZE: usize = 4;
const TOP_PAD: f32 = 4.0;
const GUTTER_PAD: f32 = 8.0;

pub struct Tab {
    pub doc: Document,
    /// Character index to apply before the next frame (navigation).
    pub pending_cursor: Option<usize>,
    /// Set by the file watcher when disk and buffer disagree.
    pub external_change: Option<String>,
    pub cursor_line: usize,
    pub cursor_col: usize,
    /// Character ranges highlighted by the search panel.
    pub search_matches: Vec<(usize, usize)>,
    pub current_match: Option<usize>,
    /// Fold start line -> fold end line (0-based, inclusive).
    folded: BTreeMap<usize, usize>,
    /// Anchor for Alt+drag column selection: (doc line, visual column).
    box_anchor: Option<(usize, usize)>,

    highlight_revision: u64,
    line_states: Vec<HighlightState>,
    line_count: usize,
    focus_pending: bool,
    scroll_to_cursor: bool,
    content_width: f32,
    cursor_screen_rect: Option<Rect>,
    hovered_identifier: Option<(String, Pos2, usize)>,
    last_status_cursor: usize,
    galleys: HashMap<usize, Arc<egui::Galley>>,
    galley_revision: u64,
    galley_font_size: f32,
}

impl Tab {
    pub fn new(doc: Document) -> Self {
        let line_count = doc.line_count();
        let (cursor_line, cursor_col) = doc.cursor_line_col();
        let mut tab = Self {
            doc,
            pending_cursor: None,
            external_change: None,
            cursor_line,
            cursor_col,
            search_matches: Vec::new(),
            current_match: None,
            folded: BTreeMap::new(),
            box_anchor: None,
            highlight_revision: u64::MAX,
            line_states: Vec::new(),
            line_count,
            focus_pending: true,
            scroll_to_cursor: false,
            content_width: 0.0,
            cursor_screen_rect: None,
            hovered_identifier: None,
            last_status_cursor: usize::MAX,
            galleys: HashMap::new(),
            galley_revision: u64::MAX,
            galley_font_size: 0.0,
        };
        tab.rebuild_highlight_states();
        tab
    }

    pub fn title(&self) -> String {
        self.doc
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.doc.path.display().to_string())
    }

    pub fn set_search_matches(&mut self, matches: Vec<(usize, usize)>, current: Option<usize>) {
        self.search_matches = matches;
        self.current_match = current;
    }

    pub fn clear_search_matches(&mut self) {
        self.search_matches.clear();
        self.current_match = None;
    }

    pub fn focus(&mut self) {
        self.focus_pending = true;
    }

    /// Select a range without necessarily stealing keyboard focus (the search
    /// panel keeps focus while the user types).
    pub fn reveal_range(&mut self, start: usize, end: usize, focus_editor: bool) {
        self.unfold_line(self.doc.line_of_char(start));
        self.unfold_line(self.doc.line_of_char(end));
        self.doc.set_selection(start, end);
        self.scroll_to_cursor = true;
        if focus_editor {
            self.focus_pending = true;
        }
    }

    pub fn reveal_cursor(&mut self, index: usize, focus_editor: bool) {
        self.unfold_line(self.doc.line_of_char(index));
        self.doc.set_cursor(index, false);
        self.scroll_to_cursor = true;
        if focus_editor {
            self.focus_pending = true;
        }
    }

    pub fn invalidate_highlight(&mut self) {
        self.highlight_revision = u64::MAX;
        self.galley_revision = u64::MAX;
        self.folded.clear();
    }

    /// Rebuild token/galley caches after a palette change without losing folds.
    pub fn invalidate_theme(&mut self) {
        self.highlight_revision = u64::MAX;
        self.galley_revision = u64::MAX;
    }

    pub fn folded_ranges(&self) -> Vec<(usize, usize)> {
        self.folded
            .iter()
            .map(|(start, end)| (*start, *end))
            .collect()
    }

    pub fn cursor_screen_rect(&self) -> Option<Rect> {
        self.cursor_screen_rect
    }

    pub fn hovered_identifier(&self) -> Option<(&str, Pos2, usize)> {
        self.hovered_identifier
            .as_ref()
            .map(|(word, position, start)| (word.as_str(), *position, *start))
    }

    pub fn restore_folds(&mut self, ranges: &[(usize, usize)]) {
        self.folded.clear();
        let mut ranges = ranges.to_vec();
        ranges.sort_unstable();
        for (start, end) in ranges {
            if start >= end || end >= self.doc.line_count() {
                continue;
            }
            if self
                .folded
                .iter()
                .any(|(known_start, known_end)| start <= *known_end && end >= *known_start)
            {
                continue;
            }
            self.folded.insert(start, end);
        }
        self.scroll_to_cursor = true;
    }

    pub fn reload_from_disk(&mut self) -> anyhow::Result<()> {
        self.doc.reload()?;
        self.external_change = None;
        self.invalidate_highlight();
        self.scroll_to_cursor = true;
        Ok(())
    }

    pub fn show(
        &mut self,
        ui: &mut Ui,
        font_size: f32,
        breakpoints: &[usize],
        debug_line: Option<usize>,
    ) -> Option<usize> {
        self.ensure_highlight_states();
        egui::ScrollArea::both()
            .id_salt(("reclipse_editor", &self.doc.path))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.paint_body(ui, font_size, breakpoints, debug_line)
            })
            .inner
    }

    fn paint_body(
        &mut self,
        ui: &mut Ui,
        font_size: f32,
        breakpoints: &[usize],
        debug_line: Option<usize>,
    ) -> Option<usize> {
        let font = FontId::monospace(font_size);
        let char_width = ui.fonts(|fonts| fonts.glyph_width(&font, 'M')).max(1.0);
        let row_height = ui.fonts(|fonts| fonts.row_height(&font)).max(1.0);
        let line_count = self.doc.line_count();
        let visible_count = self.visible_line_count().max(1);
        let digits = line_count.to_string().len().max(3);
        let gutter_width = digits as f32 * char_width + GUTTER_PAD * 2.0 + 14.0;

        let available = ui.available_width().max(320.0);
        let content_width = self
            .content_width
            .max(available - gutter_width)
            .clamp(200.0, 60_000.0);
        let total_height = TOP_PAD * 2.0 + visible_count as f32 * row_height;

        let (rect, _) = ui.allocate_exact_size(
            Vec2::new(content_width + gutter_width, total_height),
            Sense::hover(),
        );
        let editor_id = ui.make_persistent_id(("reclipse_editor_body", &self.doc.path));
        let response = ui.interact(rect, editor_id, Sense::click_and_drag());
        let clip = ui.clip_rect().intersect(rect);
        if !clip.is_positive() {
            return None;
        }
        self.cursor_screen_rect = None;

        if let Some(index) = self.pending_cursor.take() {
            self.doc.set_cursor(index, false);
            self.focus_pending = true;
            self.scroll_to_cursor = true;
        }
        if self.focus_pending {
            ui.memory_mut(|memory| memory.request_focus(editor_id));
            self.focus_pending = false;
            self.scroll_to_cursor = true;
        }

        let text_left = rect.left() + gutter_width + GUTTER_PAD;
        let pointer = response.interact_pointer_pos();
        let mut handled_gutter = false;
        let mut toggled_breakpoint = None;
        if (response.clicked() || response.drag_started())
            && pointer.is_some_and(|pos| pos.x < clip.left() + gutter_width)
        {
            if let Some(pos) = pointer {
                let visual =
                    (((pos.y - rect.top() - TOP_PAD) / row_height).floor()).max(0.0) as usize;
                let line = self.visual_to_doc(visual);
                let gutter_x = pos.x - clip.left();
                if (13.0..28.0).contains(&gutter_x) && self.is_fold_marker(line) {
                    self.toggle_fold(line);
                    handled_gutter = true;
                } else if response.clicked() {
                    toggled_breakpoint = Some(line);
                    handled_gutter = true;
                } else {
                    handled_gutter = true;
                }
            }
        }

        if !handled_gutter {
            let alt = ui.input(|input| input.modifiers.alt);
            if response.drag_started() && alt {
                response.request_focus();
                if let Some(pos) = pointer {
                    let (line, visual) =
                        self.pointer_line_visual(pos, rect, text_left, row_height, char_width);
                    self.box_anchor = Some((line, visual));
                    let text = self.doc.line_text(line);
                    let column = visual_to_char_col(&text, visual);
                    self.doc.set_cursor_states(vec![CursorState::collapsed(
                        self.doc.line_start(line) + column,
                    )]);
                    self.scroll_to_cursor = false;
                }
            } else if response.dragged() && alt && self.box_anchor.is_some() {
                if let Some(pos) = pointer {
                    let (line, visual) =
                        self.pointer_line_visual(pos, rect, text_left, row_height, char_width);
                    let (anchor_line, anchor_visual) = self.box_anchor.unwrap_or((line, visual));
                    let ranges = self.column_ranges(anchor_line, anchor_visual, line, visual);
                    self.doc.set_multi_selection(ranges);
                    self.scroll_to_cursor = false;
                }
            } else {
                if !alt {
                    self.box_anchor = None;
                }
                if response.double_clicked() {
                    if let Some(pos) = pointer {
                        let target = self.pos_to_char(pos, rect, text_left, row_height, char_width);
                        if let Some((start, end)) = self.doc.word_range_at(target) {
                            self.doc.set_selection(start, end);
                        }
                        self.scroll_to_cursor = false;
                    }
                } else if response.clicked() || response.drag_started() {
                    response.request_focus();
                    if let Some(pos) = pointer {
                        let target = self.pos_to_char(pos, rect, text_left, row_height, char_width);
                        if alt {
                            self.doc.add_cursor(CursorState::collapsed(target));
                        } else {
                            let extend = ui.input(|input| input.modifiers.shift);
                            self.doc.set_cursor(target, extend);
                        }
                        self.scroll_to_cursor = false;
                    }
                } else if response.dragged() {
                    if let Some(pos) = pointer {
                        let target = self.pos_to_char(pos, rect, text_left, row_height, char_width);
                        self.doc.set_cursor(target, true);
                        self.scroll_to_cursor = false;
                    }
                }
            }
            if response.drag_stopped() {
                self.box_anchor = None;
            }
        }

        let focused = response.has_focus() || ui.memory(|memory| memory.has_focus(editor_id));
        if focused {
            self.handle_keyboard(ui);
        }
        let cursor_line = self.doc.line_of_char(self.doc.cursor_char);
        self.unfold_line(cursor_line);

        if self.doc.revision != self.highlight_revision {
            self.ensure_highlight_states();
        }
        self.update_status_cursor();

        let visible_count = self.visible_line_count().max(1);
        let first_visible =
            (((clip.top() - (rect.top() + TOP_PAD)) / row_height).floor()).max(0.0) as usize;
        let last_visible =
            ((((clip.bottom() - (rect.top() + TOP_PAD)) / row_height).ceil()) as usize + 1)
                .min(visible_count);
        let first_visible = first_visible.min(visible_count.saturating_sub(1));

        if self.galley_revision != self.doc.revision
            || (self.galley_font_size - font_size).abs() > f32::EPSILON
        {
            self.galleys.clear();
            self.galley_revision = self.doc.revision;
            self.galley_font_size = font_size;
        }

        let painter = ui.painter();
        let gutter_rect = Rect::from_min_max(
            Pos2::new(clip.left(), clip.top()),
            Pos2::new(clip.left() + gutter_width, clip.bottom()),
        );
        painter.rect_filled(gutter_rect, 0.0, theme::panel());
        painter.line_segment(
            [
                Pos2::new(gutter_rect.right(), clip.top()),
                Pos2::new(gutter_rect.right(), clip.bottom()),
            ],
            Stroke::new(1.0_f32, theme::border()),
        );

        let text_clip = Rect::from_min_max(
            Pos2::new(clip.left() + gutter_width + 1.0, clip.top()),
            clip.max,
        );
        let text_painter = painter.with_clip_rect(text_clip);
        let cursors = self.doc.cursor_states();
        let cursor_column = self
            .doc
            .cursor_char
            .saturating_sub(self.doc.line_start(cursor_line));
        let has_selection = cursors.iter().any(|cursor| cursor.cursor != cursor.anchor);
        let time = ui.input(|input| input.time);
        let blink_on = ((time * 2.0) as i64).even();
        let gutter_text = FontId::monospace(font_size - 0.5);

        let mut widest = 0_usize;
        let mut visual = first_visible;
        let mut line = self.visual_to_doc(first_visible);
        while visual < last_visible && line < line_count {
            let line_start = self.doc.line_start(line);
            let text = self.doc.line_text(line);
            let char_count = text.chars().count();
            let display = expand_tabs(&text);
            widest = widest.max(display.chars().count());
            let y = rect.top() + TOP_PAD + visual as f32 * row_height;

            if line == cursor_line {
                painter.rect_filled(
                    Rect::from_min_max(
                        Pos2::new(text_clip.left(), y),
                        Pos2::new(text_clip.right(), y + row_height),
                    ),
                    0.0,
                    with_alpha(theme::accent_dim(), 90),
                );
            }
            if debug_line == Some(line) {
                painter.rect_filled(
                    Rect::from_min_max(
                        Pos2::new(text_clip.left(), y),
                        Pos2::new(text_clip.right(), y + row_height),
                    ),
                    0.0,
                    with_alpha(theme::yellow(), 55),
                );
            }

            for cursor in &cursors {
                let (selection_start, selection_end) = cursor.range();
                if selection_start < selection_end
                    && selection_start < line_start + char_count + 1
                    && selection_end > line_start
                {
                    let start_col = selection_start.saturating_sub(line_start).min(char_count);
                    let end_col = selection_end.saturating_sub(line_start).min(char_count);
                    let x_start = text_left + visual_col(&text, start_col) as f32 * char_width;
                    let mut x_end = text_left + visual_col(&text, end_col) as f32 * char_width;
                    if selection_end > line_start + char_count {
                        x_end += char_width * 0.75;
                    }
                    painter.rect_filled(
                        Rect::from_min_max(
                            Pos2::new(x_start, y),
                            Pos2::new(x_end.max(x_start + 1.0), y + row_height),
                        ),
                        0.0,
                        with_alpha(theme::accent(), 70),
                    );
                }
            }

            for (match_index, (match_start, match_end)) in self.search_matches.iter().enumerate() {
                if *match_start < line_start + char_count + 1 && *match_end > line_start {
                    let current = self.current_match == Some(match_index);
                    let start_col = match_start.saturating_sub(line_start).min(char_count);
                    let end_col = match_end.saturating_sub(line_start).min(char_count);
                    let x_start = text_left + visual_col(&text, start_col) as f32 * char_width;
                    let x_end = text_left + visual_col(&text, end_col) as f32 * char_width;
                    let fill = if current {
                        with_alpha(theme::yellow(), 90)
                    } else {
                        with_alpha(theme::yellow(), 40)
                    };
                    let match_rect = Rect::from_min_max(
                        Pos2::new(x_start, y),
                        Pos2::new(x_end.max(x_start + 1.0), y + row_height),
                    );
                    painter.rect_filled(match_rect, 2.0, fill);
                    if current {
                        painter.rect_stroke(
                            match_rect,
                            2.0,
                            Stroke::new(1.0_f32, theme::yellow()),
                            egui::StrokeKind::Outside,
                        );
                    }
                }
            }

            let state = self
                .line_states
                .get(line)
                .copied()
                .unwrap_or(HighlightState::Normal);
            let galley = if let Some(galley) = self.galleys.get(&line) {
                galley.clone()
            } else {
                let mut state = state;
                let tokens = tokenize_line(&display, &mut state);
                let job = build_line_job(&display, &tokens, &font);
                let galley = ui.fonts(|fonts| fonts.layout_job(job));
                self.galleys.insert(line, galley.clone());
                galley
            };
            text_painter.galley(Pos2::new(text_left, y), galley, theme::text());

            let gutter_color = if line == cursor_line {
                theme::accent()
            } else {
                theme::muted()
            };
            painter.text(
                Pos2::new(gutter_rect.right() - GUTTER_PAD, y),
                Align2::RIGHT_TOP,
                (line + 1).to_string(),
                gutter_text.clone(),
                gutter_color,
            );
            if breakpoints.contains(&line) {
                painter.circle_filled(
                    Pos2::new(gutter_rect.left() + 7.0, y + row_height * 0.5),
                    3.5,
                    theme::red(),
                );
            }
            if debug_line == Some(line) {
                let cx = gutter_rect.left() + 6.0;
                let cy = y + row_height * 0.5;
                painter.add(egui::Shape::convex_polygon(
                    vec![
                        Pos2::new(cx - 3.0, cy - 4.0),
                        Pos2::new(cx + 4.0, cy),
                        Pos2::new(cx - 3.0, cy + 4.0),
                    ],
                    theme::yellow(),
                    Stroke::NONE,
                ));
            }
            if self.is_fold_marker(line) {
                let folded = self.folded.contains_key(&line);
                let cx = gutter_rect.left() + 18.0;
                let cy = y + row_height * 0.5;
                let points = if folded {
                    vec![
                        Pos2::new(cx, cy - 4.0),
                        Pos2::new(cx + 7.0, cy),
                        Pos2::new(cx, cy + 4.0),
                    ]
                } else {
                    vec![
                        Pos2::new(cx, cy - 3.0),
                        Pos2::new(cx + 7.0, cy - 3.0),
                        Pos2::new(cx + 3.5, cy + 3.0),
                    ]
                };
                painter.add(egui::Shape::convex_polygon(
                    points,
                    theme::muted(),
                    Stroke::NONE,
                ));
            }

            if let Some(end) = self.folded.get(&line).copied() {
                line = end + 1;
            } else {
                line += 1;
            }
            visual += 1;
        }

        self.content_width = self
            .content_width
            .max(widest as f32 * char_width + 120.0)
            .min(60_000.0);

        if focused && (blink_on || has_selection) {
            for cursor in &cursors {
                let cursor_line = self.doc.line_of_char(cursor.cursor);
                if self
                    .folded
                    .iter()
                    .any(|(start, end)| *start < cursor_line && cursor_line <= *end)
                {
                    continue;
                }
                let cursor_text = self.doc.line_text(cursor_line);
                let cursor_col = cursor
                    .cursor
                    .saturating_sub(self.doc.line_start(cursor_line));
                let cursor_x = text_left + visual_col(&cursor_text, cursor_col) as f32 * char_width;
                let cursor_y =
                    rect.top() + TOP_PAD + self.doc_to_visual(cursor_line) as f32 * row_height;
                let cursor_rect =
                    Rect::from_min_size(Pos2::new(cursor_x, cursor_y), Vec2::new(2.0, row_height));
                if cursor.cursor == self.doc.cursor_char {
                    self.cursor_screen_rect = Some(cursor_rect);
                }
                text_painter.rect_filled(cursor_rect, 0.0, theme::accent());
            }
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(500));
        }

        if self.scroll_to_cursor {
            let cursor_text = self.doc.line_text(cursor_line);
            let cursor_x = text_left + visual_col(&cursor_text, cursor_column) as f32 * char_width;
            let cursor_y =
                rect.top() + TOP_PAD + self.doc_to_visual(cursor_line) as f32 * row_height;
            ui.scroll_to_rect(
                Rect::from_min_size(
                    Pos2::new(cursor_x - 24.0, cursor_y),
                    Vec2::new(char_width * 48.0, row_height),
                ),
                Some(Align::Center),
            );
            self.scroll_to_cursor = false;
        }

        let hovered = if response.dragged() {
            None
        } else {
            ui.ctx()
                .pointer_hover_pos()
                .filter(|position| rect.contains(*position))
                .and_then(|position| {
                    let index = self.pos_to_char(position, rect, text_left, row_height, char_width);
                    self.doc
                        .identifier_at(index)
                        .map(|(word, start, _)| (word, position, start))
                })
        };
        self.hovered_identifier = hovered;
        toggled_breakpoint
    }

    #[allow(clippy::too_many_lines)]
    fn handle_keyboard(&mut self, ui: &mut Ui) {
        let events = ui.input(|input| input.events.clone());
        let before_revision = self.doc.revision;

        for event in events {
            match event {
                egui::Event::Text(text) => {
                    if !text.is_empty() && !text.chars().any(|character| character.is_control()) {
                        self.doc.insert(&text);
                        self.scroll_to_cursor = true;
                    }
                }
                egui::Event::Paste(text) => {
                    let text = text.replace("\r\n", "\n");
                    self.doc.insert(&text);
                    self.scroll_to_cursor = true;
                }
                egui::Event::Copy => {
                    if let Some(text) = self.doc.selections_text() {
                        ui.ctx().copy_text(text);
                    }
                }
                egui::Event::Cut => {
                    if let Some(text) = self.doc.selections_text() {
                        ui.ctx().copy_text(text);
                        self.doc.delete_selections();
                        self.scroll_to_cursor = true;
                    }
                }
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    repeat,
                    ..
                } => {
                    let handled = self.handle_key(key, modifiers, repeat);
                    self.scroll_to_cursor = self.scroll_to_cursor || handled;
                }
                _ => {}
            }
        }

        if self.doc.revision != before_revision {
            self.ensure_highlight_states();
        }
    }

    fn handle_key(&mut self, key: egui::Key, modifiers: egui::Modifiers, _repeat: bool) -> bool {
        let command = modifiers.command;
        if command && modifiers.shift {
            match key {
                egui::Key::OpenBracket => {
                    self.ensure_highlight_states();
                    let line = self.doc.line_of_char(self.doc.cursor_char);
                    if self
                        .folded
                        .iter()
                        .any(|(start, end)| *start < line && line <= *end)
                    {
                        self.unfold_line(line);
                    } else {
                        self.toggle_fold(line);
                    }
                    return true;
                }
                egui::Key::CloseBracket => {
                    let line = self.doc.line_of_char(self.doc.cursor_char);
                    self.unfold_line(line);
                    return true;
                }
                egui::Key::Num0 => {
                    self.collapse_all_folds();
                    return true;
                }
                egui::Key::Num9 => {
                    self.folded.clear();
                    return true;
                }
                _ => {}
            }
        }
        if command && modifiers.alt {
            let delta = match key {
                egui::Key::ArrowDown => 1,
                egui::Key::ArrowUp => -1,
                _ => 0,
            };
            if delta != 0 {
                self.doc.add_cursor_vertical(delta);
                return false;
            }
        }
        if key == egui::Key::Escape {
            let cursor = self.doc.cursor_char;
            self.doc.set_cursor(cursor, false);
            return false;
        }
        if command {
            match key {
                egui::Key::Z if modifiers.shift => {
                    self.doc.redo();
                    return true;
                }
                egui::Key::Z => {
                    self.doc.undo();
                    return true;
                }
                egui::Key::Y => {
                    self.doc.redo();
                    return true;
                }
                egui::Key::A => {
                    self.doc.select_all();
                    return false;
                }
                _ => {}
            }
        }

        let extend = modifiers.shift;
        let multi = !self.doc.extra_cursors.is_empty();
        if multi {
            match key {
                egui::Key::ArrowLeft => {
                    self.doc.move_all_horizontal(-1, extend);
                    return true;
                }
                egui::Key::ArrowRight => {
                    self.doc.move_all_horizontal(1, extend);
                    return true;
                }
                egui::Key::ArrowUp => {
                    self.doc.move_all_vertical(-1, extend);
                    return true;
                }
                egui::Key::ArrowDown => {
                    self.doc.move_all_vertical(1, extend);
                    return true;
                }
                egui::Key::Home => {
                    self.doc.move_all_home(extend);
                    return true;
                }
                egui::Key::End => {
                    self.doc.move_all_end(extend);
                    return true;
                }
                egui::Key::PageUp => {
                    self.doc.move_all_vertical(-20, extend);
                    return true;
                }
                egui::Key::PageDown => {
                    self.doc.move_all_vertical(20, extend);
                    return true;
                }
                _ => {}
            }
        }

        let (line, column) = self.doc.cursor_line_col();
        match key {
            egui::Key::ArrowLeft => {
                let target = word_left(&self.doc, self.doc.cursor_char, modifiers.ctrl);
                self.doc.set_cursor(target, extend);
            }
            egui::Key::ArrowRight => {
                let target = word_right(&self.doc, self.doc.cursor_char, modifiers.ctrl);
                self.doc.set_cursor(target, extend);
            }
            egui::Key::ArrowUp => {
                let target = self.vertical_target(line, column, -1);
                self.doc.set_cursor(target, extend);
            }
            egui::Key::ArrowDown => {
                let target = self.vertical_target(line, column, 1);
                self.doc.set_cursor(target, extend);
            }
            egui::Key::Home => {
                let target = self.doc.line_start(line - 1);
                self.doc.set_cursor(target, extend);
            }
            egui::Key::End => {
                let text = self.doc.line_text(line - 1);
                let target = self.doc.line_start(line - 1) + text.chars().count();
                self.doc.set_cursor(target, extend);
            }
            egui::Key::PageUp => {
                let target = self.vertical_target(line, column, -20);
                self.doc.set_cursor(target, extend);
            }
            egui::Key::PageDown => {
                let target = self.vertical_target(line, column, 20);
                self.doc.set_cursor(target, extend);
            }
            egui::Key::Backspace => {
                self.doc.backspace();
            }
            egui::Key::Delete => {
                self.doc.delete_forward();
            }
            egui::Key::Enter => {
                let indent = current_indent(&self.doc, line - 1);
                let extra = if self.line_before_cursor_opens_block() {
                    "    "
                } else {
                    ""
                };
                let insertion = format!("\n{indent}{extra}");
                self.doc.insert(&insertion);
            }
            egui::Key::Tab => {
                if modifiers.shift {
                    self.unindent_current_line();
                } else {
                    self.doc.insert("    ");
                }
            }
            egui::Key::Escape => {
                let cursor = self.doc.cursor_char;
                self.doc.set_cursor(cursor, false);
            }
            _ => return false,
        }
        true
    }

    fn vertical_target(&self, line: usize, column: usize, delta: i32) -> usize {
        let target_line = (line as i32 + delta).clamp(1, self.doc.line_count() as i32) as usize;
        self.doc.char_index_for_line_col(target_line, column)
    }

    fn line_before_cursor_opens_block(&self) -> bool {
        let line = self.doc.line_of_char(self.doc.cursor_char);
        let text = self.doc.line_text(line);
        text.trim_end().ends_with('{')
    }

    fn unindent_current_line(&mut self) {
        let line = self.doc.line_of_char(self.doc.cursor_char);
        let start = self.doc.line_start(line);
        let text = self.doc.line_text(line);
        let remove = text
            .chars()
            .take(4)
            .take_while(|character| *character == ' ' || *character == '\t')
            .count();
        if remove > 0 {
            self.doc.edit(start, start + remove, "");
        }
    }

    fn update_status_cursor(&mut self) {
        if self.doc.cursor_char != self.last_status_cursor {
            let (line, column) = self.doc.cursor_line_col();
            self.cursor_line = line;
            self.cursor_col = column;
            self.last_status_cursor = self.doc.cursor_char;
        }
    }

    fn pos_to_char(
        &self,
        pos: Pos2,
        rect: Rect,
        text_left: f32,
        row_height: f32,
        char_width: f32,
    ) -> usize {
        let (line, visual_col) =
            self.pointer_line_visual(pos, rect, text_left, row_height, char_width);
        let text = self.doc.line_text(line);
        let column = visual_to_char_col(&text, visual_col);
        self.doc.line_start(line) + column
    }

    fn pointer_line_visual(
        &self,
        pos: Pos2,
        rect: Rect,
        text_left: f32,
        row_height: f32,
        char_width: f32,
    ) -> (usize, usize) {
        let visual = (((pos.y - rect.top() - TOP_PAD) / row_height).floor()).max(0.0) as usize;
        let line = self.visual_to_doc(visual);
        let visual_col = (((pos.x - text_left) / char_width).round()).max(0.0) as usize;
        (line, visual_col)
    }

    fn column_ranges(
        &self,
        anchor_line: usize,
        anchor_visual: usize,
        current_line: usize,
        current_visual: usize,
    ) -> Vec<(usize, usize)> {
        let (first, last) = if anchor_line <= current_line {
            (anchor_line, current_line)
        } else {
            (current_line, anchor_line)
        };
        let (min_visual, max_visual) = if anchor_visual <= current_visual {
            (anchor_visual, current_visual)
        } else {
            (current_visual, anchor_visual)
        };
        let mut ranges = Vec::with_capacity(last - first + 1);
        for line in first..=last {
            let text = self.doc.line_text(line);
            let start_col = visual_to_char_col(&text, min_visual);
            let end_col = visual_to_char_col(&text, max_visual);
            let line_start = self.doc.line_start(line);
            ranges.push((line_start + start_col, line_start + end_col));
        }
        ranges
    }

    fn hidden_lines(&self) -> usize {
        self.folded
            .iter()
            .map(|(start, end)| end.saturating_sub(*start))
            .sum()
    }

    fn visible_line_count(&self) -> usize {
        self.line_count.saturating_sub(self.hidden_lines())
    }

    fn visual_to_doc(&self, visual: usize) -> usize {
        let mut remaining = visual;
        let mut doc_line = 0;
        for (&start, &end) in &self.folded {
            if start < doc_line {
                continue;
            }
            let gap = start.saturating_sub(doc_line);
            if remaining < gap {
                return (doc_line + remaining).min(self.line_count.saturating_sub(1));
            }
            remaining -= gap;
            doc_line = start;
            if remaining == 0 {
                return doc_line;
            }
            remaining -= 1;
            doc_line = end + 1;
            if doc_line >= self.line_count {
                return self.line_count.saturating_sub(1);
            }
        }
        (doc_line + remaining).min(self.line_count.saturating_sub(1))
    }

    fn doc_to_visual(&self, doc_line: usize) -> usize {
        let mut hidden = 0;
        for (&start, &end) in &self.folded {
            if start < doc_line {
                hidden += (end - start).min(doc_line - start);
            } else {
                break;
            }
        }
        doc_line.saturating_sub(hidden)
    }

    fn unfold_line(&mut self, line: usize) {
        let fold = self
            .folded
            .iter()
            .find(|(start, end)| **start < line && line <= **end)
            .map(|(start, _)| *start);
        if let Some(start) = fold {
            self.folded.remove(&start);
        }
    }

    fn toggle_fold(&mut self, line: usize) {
        if self.folded.remove(&line).is_some() {
            return;
        }
        let Some(end) = self.fold_end_for_line(line) else {
            return;
        };
        // Keep fold ranges non-overlapping. Folding a parent collapses any
        // child folds; folding a child inside an expanded parent is fine.
        self.folded
            .retain(|&start, fold_end| *fold_end < line || start > end);
        self.folded.insert(line, end);
    }

    fn fold_end_for_line(&self, start: usize) -> Option<usize> {
        let line_count = self.doc.line_count();
        if start + 1 >= line_count {
            return None;
        }
        let first = self.doc.line_text(start);
        if !first.trim_end().ends_with('{') {
            return None;
        }

        let mut state = self
            .line_states
            .get(start)
            .copied()
            .unwrap_or(HighlightState::Normal);
        let mut depth = 0_i32;
        for line in start..line_count {
            let text = self.doc.line_text(line);
            let tokens = tokenize_line(&text, &mut state);
            let (opens, closes) = brace_counts(&text, &tokens);
            depth += opens as i32 - closes as i32;
            if line == start && depth <= 0 {
                return None;
            }
            if line > start && depth <= 0 {
                return Some(line);
            }
        }
        None
    }

    fn is_fold_marker(&self, line: usize) -> bool {
        self.folded.contains_key(&line) || self.doc.line_text(line).trim_end().ends_with('{')
    }

    fn collapse_all_folds(&mut self) {
        self.ensure_highlight_states();
        self.folded.clear();
        let line_count = self.doc.line_count();
        let mut line = 0;
        while line + 1 < line_count {
            if self.doc.line_text(line).trim_end().ends_with('{') {
                if let Some(end) = self.fold_end_for_line(line) {
                    self.folded.insert(line, end);
                    line = end + 1;
                    continue;
                }
            }
            line += 1;
        }
    }

    fn ensure_highlight_states(&mut self) {
        if self.highlight_revision == self.doc.revision {
            self.line_count = self.doc.line_count();
            return;
        }
        let new_count = self.doc.line_count();
        let old_count = self.line_states.len();
        if old_count == 0 || self.highlight_revision == u64::MAX {
            self.rebuild_highlight_states();
            return;
        }

        let mut delta = self.doc.last_edit_line_delta;
        if old_count as isize + delta != new_count as isize {
            delta = new_count as isize - old_count as isize;
        }
        if new_count != old_count {
            self.folded.clear();
        }
        let start = self
            .doc
            .last_edit_line
            .min(old_count.saturating_sub(1))
            .min(new_count.saturating_sub(1));

        let mut states = vec![HighlightState::Normal; new_count];
        let prefix = (start + 1).min(old_count).min(new_count);
        states[..prefix].copy_from_slice(&self.line_states[..prefix]);
        let mut state = states
            .get(start)
            .copied()
            .or_else(|| self.line_states.last().copied())
            .unwrap_or(HighlightState::Normal);

        if delta > 0 {
            let inserted = delta as usize;
            let affected_end = (start + inserted).min(new_count.saturating_sub(1));
            let mut line = start;
            while line + 1 < new_count {
                tokenize_line(&self.doc.line_text(line), &mut state);
                states[line + 1] = state;
                if line >= affected_end {
                    let old_index = line + 1 - inserted;
                    if old_index < old_count && state == self.line_states[old_index] {
                        let remaining = new_count - (line + 1);
                        if old_index + remaining <= old_count {
                            states[line + 1..].copy_from_slice(
                                &self.line_states[old_index..old_index + remaining],
                            );
                            break;
                        }
                    }
                }
                line += 1;
            }
        } else if delta < 0 {
            let removed = (-delta) as usize;
            let mut line = start;
            while line + 1 < new_count {
                tokenize_line(&self.doc.line_text(line), &mut state);
                states[line + 1] = state;
                let old_index = line + 1 + removed;
                if old_index < old_count && state == self.line_states[old_index] {
                    let remaining = new_count - (line + 1);
                    if old_index + remaining <= old_count {
                        states[line + 1..]
                            .copy_from_slice(&self.line_states[old_index..old_index + remaining]);
                        break;
                    }
                }
                line += 1;
            }
        } else {
            let mut line = start;
            while line + 1 < new_count {
                let previous = self.line_states[line + 1];
                tokenize_line(&self.doc.line_text(line), &mut state);
                states[line + 1] = state;
                if state == previous {
                    states[line + 1..].copy_from_slice(&self.line_states[line + 1..]);
                    break;
                }
                line += 1;
            }
        }

        self.line_states = states;
        self.line_count = new_count;
        self.highlight_revision = self.doc.revision;
        self.galleys.clear();
        self.galley_revision = u64::MAX;
    }

    fn rebuild_highlight_states(&mut self) {
        self.line_count = self.doc.line_count();
        self.line_states = vec![HighlightState::Normal; self.line_count];
        for line in 0..self.line_count.saturating_sub(1) {
            let mut state = self.line_states[line];
            tokenize_line(&self.doc.line_text(line), &mut state);
            self.line_states[line + 1] = state;
        }
        self.highlight_revision = self.doc.revision;
        self.galleys.clear();
        self.galley_revision = u64::MAX;
    }
}

fn build_line_job(display: &str, tokens: &[TokenSpan], font: &FontId) -> LayoutJob {
    let mut job = LayoutJob {
        text: display.to_owned(),
        wrap: egui::text::TextWrapping {
            max_width: f32::INFINITY,
            ..Default::default()
        },
        break_on_newline: false,
        ..Default::default()
    };

    let mut cursor = 0;
    for token in tokens {
        if token.start > cursor {
            push_section(&mut job, cursor, token.start, plain_format(font));
        }
        push_section(
            &mut job,
            token.start,
            token.end,
            format_for(token.kind, font, token.kind == TokenKind::Comment),
        );
        cursor = token.end;
    }
    if cursor < display.len() {
        push_section(&mut job, cursor, display.len(), plain_format(font));
    }
    job
}

fn push_section(job: &mut LayoutJob, start: usize, end: usize, format: TextFormat) {
    if start >= end {
        return;
    }
    job.sections.push(LayoutSection {
        leading_space: 0.0,
        byte_range: start..end,
        format,
    });
}

fn with_alpha(color: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha)
}

fn format_for(kind: TokenKind, font: &FontId, italic: bool) -> TextFormat {
    let (color, italics) = match kind {
        TokenKind::Keyword => (theme::clr_keyword(), false),
        TokenKind::TypeName => (theme::clr_type(), false),
        TokenKind::String | TokenKind::Char => (theme::clr_string(), false),
        TokenKind::Comment => (theme::clr_comment(), true),
        TokenKind::Number => (theme::clr_number(), false),
        TokenKind::Annotation => (theme::clr_annotation(), false),
        TokenKind::Literal => (theme::clr_literal(), false),
    };
    let mut format = plain_format(font);
    format.color = color;
    format.italics = italic || italics;
    format
}

fn plain_format(font: &FontId) -> TextFormat {
    TextFormat {
        font_id: font.clone(),
        color: theme::text(),
        ..Default::default()
    }
}

fn expand_tabs(line: &str) -> String {
    let mut output = String::with_capacity(line.len());
    let mut visual = 0;
    for character in line.chars() {
        if character == '\t' {
            let spaces = TAB_SIZE - (visual % TAB_SIZE);
            output.extend(std::iter::repeat_n(' ', spaces));
            visual += spaces;
        } else {
            output.push(character);
            visual += 1;
        }
    }
    output
}

fn brace_counts(line: &str, tokens: &[TokenSpan]) -> (usize, usize) {
    let mut opens = 0;
    let mut closes = 0;
    let mut token_index = 0;
    for (byte, character) in line.char_indices() {
        while token_index < tokens.len() && tokens[token_index].end <= byte {
            token_index += 1;
        }
        if token_index < tokens.len()
            && byte >= tokens[token_index].start
            && byte < tokens[token_index].end
        {
            continue;
        }
        match character {
            '{' => opens += 1,
            '}' => closes += 1,
            _ => {}
        }
    }
    (opens, closes)
}

fn visual_col(line: &str, char_col: usize) -> usize {
    let mut visual = 0;
    for character in line.chars().take(char_col) {
        if character == '\t' {
            visual += TAB_SIZE - (visual % TAB_SIZE);
        } else {
            visual += 1;
        }
    }
    visual
}

fn visual_to_char_col(line: &str, visual: usize) -> usize {
    let mut current = 0;
    for (index, character) in line.chars().enumerate() {
        let next = if character == '\t' {
            current + TAB_SIZE - (current % TAB_SIZE)
        } else {
            current + 1
        };
        if visual < next {
            return index;
        }
        current = next;
    }
    line.chars().count()
}

fn current_indent(doc: &Document, line: usize) -> String {
    doc.line_text(line)
        .chars()
        .take_while(|character| *character == ' ' || *character == '\t')
        .collect()
}

fn word_left(doc: &Document, cursor: usize, by_word: bool) -> usize {
    if !by_word || cursor == 0 {
        return cursor.saturating_sub(1);
    }
    let text: Vec<char> = doc.text().chars().collect();
    let mut index = cursor.min(text.len());
    while index > 0 && !text[index - 1].is_alphanumeric() && text[index - 1] != '_' {
        index -= 1;
    }
    while index > 0 && (text[index - 1].is_alphanumeric() || text[index - 1] == '_') {
        index -= 1;
    }
    index
}

fn word_right(doc: &Document, cursor: usize, by_word: bool) -> usize {
    let text: Vec<char> = doc.text().chars().collect();
    if !by_word || cursor >= text.len() {
        return (cursor + 1).min(text.len());
    }
    let mut index = cursor;
    while index < text.len() && (text[index].is_alphanumeric() || text[index] == '_') {
        index += 1;
    }
    while index < text.len() && !text[index].is_alphanumeric() && text[index] != '_' {
        index += 1;
    }
    index
}

trait Even {
    fn even(self) -> bool;
}

impl Even for i64 {
    fn even(self) -> bool {
        self % 2 == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn test_tab(source: &str) -> (tempfile::TempDir, Tab) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Test.java");
        fs::write(&path, source).unwrap();
        let tab = Tab::new(Document::open(path).unwrap());
        (dir, tab)
    }

    #[test]
    fn incremental_line_states_match_full_rebuild() {
        let (_dir, mut tab) = test_tab("class A {\n  /* comment\n  still */\n  int x = 1;\n}\n");
        let position = tab.doc.char_index_for_line_col(2, 1);
        tab.doc.set_cursor(position, false);
        tab.doc.insert("inserted\nline\n");
        tab.ensure_highlight_states();
        let incremental = tab.line_states.clone();
        tab.rebuild_highlight_states();
        assert_eq!(incremental, tab.line_states);
    }

    #[test]
    fn incremental_line_states_match_after_deletion() {
        let (_dir, mut tab) = test_tab("class A {\n  /* comment\n  still */\n  int x = 1;\n}\n");
        let start = tab.doc.line_start(1);
        let end = tab.doc.line_start(3);
        tab.doc.edit(start, end, "");
        tab.ensure_highlight_states();
        let incremental = tab.line_states.clone();
        tab.rebuild_highlight_states();
        assert_eq!(incremental, tab.line_states);
    }

    #[test]
    fn folds_hide_inner_lines_and_unfold() {
        let (_dir, mut tab) = test_tab("class A {\n  void run() {\n    int x = 1;\n  }\n}\n");
        assert_eq!(tab.fold_end_for_line(0), Some(4));
        tab.toggle_fold(0);
        assert!(tab.visible_line_count() < tab.doc.line_count());
        assert_eq!(tab.visual_to_doc(1), 5);
        tab.toggle_fold(0);
        assert_eq!(tab.visual_to_doc(1), 1);
    }
}
