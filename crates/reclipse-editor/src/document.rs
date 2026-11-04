//! Reclipse editor document model: rope buffer, cursors, edits and undo.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use ropey::Rope;

const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_UNDO_STEPS: usize = 512;
const EDIT_GROUP_WINDOW: Duration = Duration::from_millis(600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    Insert,
    Delete,
    Replace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorState {
    pub cursor: usize,
    pub anchor: usize,
}

impl CursorState {
    pub fn new(cursor: usize, anchor: usize) -> Self {
        Self { cursor, anchor }
    }

    pub fn collapsed(cursor: usize) -> Self {
        Self {
            cursor,
            anchor: cursor,
        }
    }

    pub fn range(self) -> (usize, usize) {
        (self.cursor.min(self.anchor), self.cursor.max(self.anchor))
    }
}

#[derive(Debug, Clone)]
struct EditDelta {
    start: usize,
    removed: String,
    inserted: String,
}
#[derive(Debug)]
struct EditRecord {
    edits: Vec<EditDelta>,
    cursors_before: Vec<CursorState>,
    cursors_after: Vec<CursorState>,
}

#[derive(Debug)]
pub struct Document {
    pub path: PathBuf,
    /// Char-indexed rope. All public positions in this type are character
    /// offsets, never byte offsets.
    pub rope: Rope,
    pub dirty: bool,
    pub revision: u64,
    pub cursor_char: usize,
    pub anchor_char: usize,
    /// Additional cursors/selections (multi-cursor and column selection).
    pub extra_cursors: Vec<CursorState>,
    /// 0-based line where the last edit started. The UI uses it to invalidate
    /// only the affected part of the lexical highlight state.
    pub last_edit_line: usize,
    /// Line-count change caused by the last edit (`new - old`).
    pub last_edit_line_delta: isize,

    undo: Vec<EditRecord>,
    redo: Vec<EditRecord>,
    last_edit_at: Option<Instant>,
    last_edit_kind: Option<EditKind>,
}

impl Document {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let metadata =
            fs::metadata(&path).with_context(|| format!("failed to stat {}", path.display()))?;
        if metadata.len() > MAX_FILE_BYTES {
            bail!(
                "file is too large ({} MB, limit {} MB)",
                metadata.len() / (1024 * 1024),
                MAX_FILE_BYTES / (1024 * 1024)
            );
        }

        let bytes =
            fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
        let mut text = String::from_utf8(bytes).map_err(|_| {
            anyhow::anyhow!(
                "{} is not valid UTF-8; binary files are not supported yet",
                path.display()
            )
        })?;

        if let Some(stripped) = text.strip_prefix('\u{feff}') {
            text = stripped.to_owned();
        }

        Ok(Self {
            path,
            rope: Rope::from_str(&text),
            dirty: false,
            revision: 0,
            cursor_char: 0,
            anchor_char: 0,
            extra_cursors: Vec::new(),
            last_edit_line: 0,
            last_edit_line_delta: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            last_edit_at: None,
            last_edit_kind: None,
        })
    }

    /// Materialize the whole buffer. Prefer rope methods in hot paths.
    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    pub fn len_chars(&self) -> usize {
        self.rope.len_chars()
    }

    pub fn line_count(&self) -> usize {
        self.rope.len_lines().max(1)
    }

    pub fn line_start(&self, line: usize) -> usize {
        self.rope
            .line_to_char(line.min(self.line_count().saturating_sub(1)))
    }

    pub fn line_text(&self, line: usize) -> String {
        if line >= self.line_count() {
            return String::new();
        }
        let mut text = self.rope.line(line).to_string();
        if let Some(stripped) = text.strip_suffix('\n') {
            text = stripped.to_owned();
        }
        if let Some(stripped) = text.strip_suffix('\r') {
            text = stripped.to_owned();
        }
        text
    }

    pub fn line_of_char(&self, char_index: usize) -> usize {
        self.rope.char_to_line(char_index.min(self.len_chars()))
    }

    pub fn line_col_at_char(&self, char_index: usize) -> (usize, usize) {
        let char_index = char_index.min(self.len_chars());
        let line = self.rope.char_to_line(char_index);
        let start = self.rope.line_to_char(line);
        (line + 1, char_index - start + 1)
    }

    pub fn char_index_for_line_col(&self, line: usize, column: usize) -> usize {
        let line = line.max(1).min(self.line_count());
        let column = column.max(1);
        let start = self.rope.line_to_char(line - 1);
        let next = if line < self.line_count() {
            self.rope.line_to_char(line)
        } else {
            self.len_chars()
        };
        let line_len = self.rope.slice(start..next).to_string();
        let line_len = line_len.trim_end_matches(['\r', '\n']).chars().count();
        start + (column - 1).min(line_len)
    }

    pub fn cursor_line_col(&self) -> (usize, usize) {
        self.line_col_at_char(self.cursor_char)
    }

    pub fn primary_cursor(&self) -> CursorState {
        CursorState::new(self.cursor_char, self.anchor_char)
    }

    pub fn cursor_states(&self) -> Vec<CursorState> {
        let mut cursors = Vec::with_capacity(1 + self.extra_cursors.len());
        cursors.push(self.primary_cursor());
        cursors.extend(self.extra_cursors.iter().copied());
        cursors
    }

    pub fn set_cursor_states(&mut self, mut cursors: Vec<CursorState>) {
        let len = self.len_chars();
        for cursor in &mut cursors {
            cursor.cursor = cursor.cursor.min(len);
            cursor.anchor = cursor.anchor.min(len);
        }
        let mut deduped: Vec<CursorState> = Vec::with_capacity(cursors.len());
        for cursor in cursors {
            if !deduped.contains(&cursor) {
                deduped.push(cursor);
            }
        }
        if deduped.is_empty() {
            self.cursor_char = self.cursor_char.min(len);
            self.anchor_char = self.anchor_char.min(len);
            self.extra_cursors.clear();
            return;
        }
        let primary = deduped.remove(0);
        self.cursor_char = primary.cursor;
        self.anchor_char = primary.anchor;
        self.extra_cursors = deduped;
    }

    pub fn add_cursor(&mut self, cursor: CursorState) {
        let len = self.len_chars();
        let cursor = CursorState::new(cursor.cursor.min(len), cursor.anchor.min(len));
        if cursor == self.primary_cursor() || self.extra_cursors.contains(&cursor) {
            return;
        }
        self.extra_cursors.push(cursor);
    }

    pub fn clear_extra_cursors(&mut self) {
        self.extra_cursors.clear();
    }

    pub fn set_multi_selection(&mut self, ranges: Vec<(usize, usize)>) {
        let cursors: Vec<CursorState> = ranges
            .into_iter()
            .map(|(start, end)| CursorState::new(end, start))
            .collect();
        self.set_cursor_states(cursors);
    }

    pub fn selection(&self) -> Option<(usize, usize)> {
        if self.cursor_char == self.anchor_char {
            None
        } else {
            Some((
                self.cursor_char.min(self.anchor_char),
                self.cursor_char.max(self.anchor_char),
            ))
        }
    }

    pub fn selection_text(&self) -> Option<String> {
        self.selection().map(|(start, end)| self.slice(start, end))
    }

    /// All non-collapsed selections, used by copy/cut in multi-cursor mode.
    pub fn selections_text(&self) -> Option<String> {
        let mut texts = Vec::new();
        for cursor in self.cursor_states() {
            let (start, end) = cursor.range();
            if start < end {
                texts.push(self.slice(start, end));
            }
        }
        if texts.is_empty() {
            None
        } else {
            Some(texts.join("\n"))
        }
    }

    pub fn slice(&self, start: usize, end: usize) -> String {
        let len = self.len_chars();
        let start = start.min(len);
        let end = end.min(len).max(start);
        self.rope.slice(start..end).to_string()
    }

    pub fn set_cursor(&mut self, char_index: usize, extend: bool) {
        let char_index = char_index.min(self.len_chars());
        self.cursor_char = char_index;
        if !extend {
            self.anchor_char = char_index;
        }
        self.extra_cursors.clear();
    }

    pub fn set_selection(&mut self, start: usize, end: usize) {
        let len = self.len_chars();
        self.anchor_char = start.min(len);
        self.cursor_char = end.min(len);
        self.extra_cursors.clear();
    }

    pub fn select_all(&mut self) {
        self.anchor_char = 0;
        self.cursor_char = self.len_chars();
        self.extra_cursors.clear();
    }

    pub fn identifier_at_cursor(&self) -> Option<(String, usize, usize)> {
        identifier_at(&self.text(), self.cursor_char)
    }

    /// Identifier at an arbitrary character index using rope slices instead of
    /// materializing the whole document.
    pub fn identifier_at(&self, char_index: usize) -> Option<(String, usize, usize)> {
        let len = self.len_chars();
        if len == 0 {
            return None;
        }
        let mut index = char_index.min(len - 1);
        if !is_identifier_char(Some(self.rope.char(index))) {
            if index > 0 && is_identifier_char(Some(self.rope.char(index - 1))) {
                index -= 1;
            } else if index + 1 < len && is_identifier_char(Some(self.rope.char(index + 1))) {
                index += 1;
            } else {
                return None;
            }
        }
        let mut start = index;
        while start > 0 && is_identifier_char(Some(self.rope.char(start - 1))) {
            start -= 1;
        }
        let mut end = index + 1;
        while end < len && is_identifier_char(Some(self.rope.char(end))) {
            end += 1;
        }
        Some((self.rope.slice(start..end).to_string(), start, end))
    }

    pub fn word_range_at(&self, char_index: usize) -> Option<(usize, usize)> {
        identifier_at(&self.text(), char_index).map(|(_, start, end)| (start, end))
    }

    /// Replace a character range and record an undoable edit. Extra cursors are
    /// discarded: callers doing multi-cursor editing use `insert`/`backspace`.
    pub fn edit(&mut self, start: usize, end: usize, inserted: &str) -> usize {
        let len = self.len_chars();
        let start = start.min(len);
        let end = end.min(len).max(start);
        if start == end && inserted.is_empty() {
            return self.cursor_char;
        }

        let removed = self.rope.slice(start..end).to_string();
        let old_line_count = self.line_count();
        let kind = if start == end {
            EditKind::Insert
        } else if inserted.is_empty() {
            EditKind::Delete
        } else {
            EditKind::Replace
        };
        let now = Instant::now();
        let new_cursor = start + inserted.chars().count();

        let mut grouped = false;
        if let Some(last) = self.undo.last_mut() {
            let recent = self
                .last_edit_at
                .is_some_and(|at| now.duration_since(at) < EDIT_GROUP_WINDOW);
            if recent
                && self.last_edit_kind == Some(kind)
                && self.redo.is_empty()
                && last.edits.len() == 1
            {
                let delta = &mut last.edits[0];
                match kind {
                    EditKind::Insert => {
                        if delta.start + delta.inserted.chars().count() == start
                            && last.cursors_after.len() == 1
                            && last.cursors_after[0].cursor == start
                        {
                            delta.inserted.push_str(inserted);
                            last.cursors_after = vec![CursorState::collapsed(
                                delta.start + delta.inserted.chars().count(),
                            )];
                            grouped = true;
                        }
                    }
                    EditKind::Delete if inserted.is_empty() => {
                        if end == delta.start && start < delta.start {
                            delta.start = start;
                            delta.removed = format!("{}{}", removed, delta.removed);
                            last.cursors_after = vec![CursorState::collapsed(start)];
                            grouped = true;
                        } else if start == delta.start
                            && end == delta.start + delta.removed.chars().count()
                        {
                            delta.removed.push_str(&removed);
                            last.cursors_after = vec![CursorState::collapsed(start)];
                            grouped = true;
                        }
                    }
                    _ => {}
                }
            }
        }

        if !grouped {
            let cursors_before = self.cursor_states();
            let cursors_after = vec![CursorState::collapsed(new_cursor)];
            self.undo.push(EditRecord {
                edits: vec![EditDelta {
                    start,
                    removed,
                    inserted: inserted.to_owned(),
                }],
                cursors_before,
                cursors_after,
            });
            if self.undo.len() > MAX_UNDO_STEPS {
                self.undo.remove(0);
            }
        }
        self.redo.clear();

        self.rope.remove(start..end);
        if !inserted.is_empty() {
            self.rope.insert(start, inserted);
        }
        self.cursor_char = new_cursor;
        self.anchor_char = new_cursor;
        self.extra_cursors.clear();
        self.dirty = true;
        self.revision += 1;
        self.last_edit_line = self.rope.char_to_line(start.min(self.len_chars()));
        self.last_edit_line_delta = self.line_count() as isize - old_line_count as isize;
        self.last_edit_at = Some(now);
        self.last_edit_kind = Some(kind);
        self.cursor_char
    }

    pub fn insert(&mut self, text: &str) -> usize {
        if self.extra_cursors.is_empty() {
            let (start, end) = self
                .selection()
                .unwrap_or((self.cursor_char, self.cursor_char));
            return self.edit(start, end, text);
        }
        self.multi_edit_selections(text)
    }

    pub fn backspace(&mut self) -> bool {
        if self.extra_cursors.is_empty() {
            if let Some((start, end)) = self.selection() {
                self.edit(start, end, "");
                return true;
            }
            if self.cursor_char == 0 {
                return false;
            }
            let target = self.cursor_char.saturating_sub(1);
            self.edit(target, self.cursor_char, "");
            return true;
        }

        let mut ranges = Vec::new();
        for cursor in self.cursor_states() {
            let (start, end) = cursor.range();
            if start < end {
                ranges.push((start, end));
            } else if cursor.cursor > 0 {
                ranges.push((cursor.cursor - 1, cursor.cursor));
            }
        }
        if ranges.is_empty() {
            return false;
        }
        self.multi_edit_ranges(ranges, "");
        true
    }

    pub fn delete_forward(&mut self) -> bool {
        if self.extra_cursors.is_empty() {
            if let Some((start, end)) = self.selection() {
                self.edit(start, end, "");
                return true;
            }
            if self.cursor_char >= self.len_chars() {
                return false;
            }
            self.edit(self.cursor_char, self.cursor_char + 1, "");
            return true;
        }

        let mut ranges = Vec::new();
        for cursor in self.cursor_states() {
            let (start, end) = cursor.range();
            if start < end {
                ranges.push((start, end));
            } else if cursor.cursor < self.len_chars() {
                ranges.push((cursor.cursor, cursor.cursor + 1));
            }
        }
        if ranges.is_empty() {
            return false;
        }
        self.multi_edit_ranges(ranges, "");
        true
    }

    pub fn delete_selections(&mut self) -> bool {
        let ranges: Vec<_> = self
            .cursor_states()
            .into_iter()
            .filter_map(|cursor| {
                let (start, end) = cursor.range();
                (start < end).then_some((start, end))
            })
            .collect();
        if ranges.is_empty() {
            return false;
        }
        self.multi_edit_ranges(ranges, "");
        true
    }

    /// Insert the same text at every cursor/selection. This is the primitive
    /// behind multi-cursor typing and column editing.
    pub fn multi_edit_selections(&mut self, inserted: &str) -> usize {
        let ranges = self
            .cursor_states()
            .into_iter()
            .map(CursorState::range)
            .collect();
        self.multi_edit_ranges(ranges, inserted)
    }

    fn multi_edit_ranges(&mut self, ranges: Vec<(usize, usize)>, inserted: &str) -> usize {
        let len = self.len_chars();
        let mut ranges: Vec<(usize, usize)> = ranges
            .into_iter()
            .map(|(start, end)| {
                let start = start.min(len);
                let end = end.min(len).max(start);
                (start, end)
            })
            .collect();
        if ranges.is_empty() {
            return self.cursor_char;
        }
        ranges.sort();
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for (start, end) in ranges {
            if let Some(last) = merged.last_mut() {
                if start < last.1 || start == last.0 {
                    last.1 = last.1.max(end);
                    continue;
                }
            }
            merged.push((start, end));
        }

        let old_line_count = self.line_count();
        let original_cursors = self.cursor_states();
        let mut edits = Vec::with_capacity(merged.len());
        let mut final_positions = Vec::with_capacity(merged.len());
        let mut offset = 0_isize;
        let inserted_len = inserted.chars().count() as isize;
        for (start, end) in &merged {
            let removed = self.rope.slice(*start..*end).to_string();
            edits.push(EditDelta {
                start: *start,
                removed,
                inserted: inserted.to_owned(),
            });
            let final_position = (*start as isize + offset + inserted_len).max(0) as usize;
            final_positions.push(final_position);
            offset += inserted_len - (*end - *start) as isize;
        }

        // Map each original cursor to the merged range containing its start.
        let mut cursors_after = Vec::with_capacity(original_cursors.len());
        for cursor in &original_cursors {
            let (start, _) = cursor.range();
            let index = merged
                .iter()
                .position(|(range_start, range_end)| start >= *range_start && start <= *range_end)
                .unwrap_or_else(|| {
                    merged
                        .iter()
                        .rposition(|(range_start, _)| *range_start <= start)
                        .unwrap_or(0)
                });
            let position = final_positions.get(index).copied().unwrap_or(start);
            cursors_after.push(CursorState::collapsed(position));
        }

        let now = Instant::now();
        let kind = if inserted.is_empty() {
            EditKind::Delete
        } else if merged.iter().all(|(start, end)| start == end) {
            EditKind::Insert
        } else {
            EditKind::Replace
        };
        let mut grouped = false;
        if kind == EditKind::Insert && !inserted.is_empty() && self.redo.is_empty() {
            if let Some(last) = self.undo.last_mut() {
                let recent = self
                    .last_edit_at
                    .is_some_and(|at| now.duration_since(at) < EDIT_GROUP_WINDOW);
                if recent
                    && self.last_edit_kind == Some(EditKind::Insert)
                    && last.edits.len() == edits.len()
                    && last.cursors_after.len() == merged.len()
                {
                    let mut cumulative = 0_isize;
                    let matches = last.edits.iter().zip(&merged).all(|(edit, (start, _))| {
                        let final_start = (edit.start as isize
                            + cumulative
                            + edit.inserted.chars().count() as isize)
                            .max(0) as usize;
                        cumulative += edit.inserted.chars().count() as isize
                            - edit.removed.chars().count() as isize;
                        final_start == *start
                    }) && last
                        .cursors_after
                        .iter()
                        .zip(&merged)
                        .all(|(cursor, (start, end))| start == end && cursor.cursor == *start);
                    if matches {
                        for (edit, _) in last.edits.iter_mut().zip(&merged) {
                            edit.inserted.push_str(inserted);
                        }
                        last.cursors_after = final_positions
                            .iter()
                            .map(|position| CursorState::collapsed(*position))
                            .collect();
                        grouped = true;
                    }
                }
            }
        }

        if !grouped {
            self.undo.push(EditRecord {
                edits: edits.clone(),
                cursors_before: original_cursors.clone(),
                cursors_after: cursors_after.clone(),
            });
            if self.undo.len() > MAX_UNDO_STEPS {
                self.undo.remove(0);
            }
        }
        self.redo.clear();

        for edit in edits.iter().rev() {
            let remove_len = edit.removed.chars().count();
            if remove_len > 0 {
                self.rope.remove(edit.start..edit.start + remove_len);
            }
            if !edit.inserted.is_empty() {
                self.rope.insert(edit.start, &edit.inserted);
            }
        }

        self.set_cursor_states(cursors_after);
        self.dirty = true;
        self.revision += 1;
        let first_start = merged.first().map(|(start, _)| *start).unwrap_or(0);
        self.last_edit_line = self.rope.char_to_line(first_start.min(self.len_chars()));
        self.last_edit_line_delta = self.line_count() as isize - old_line_count as isize;
        self.last_edit_at = Some(now);
        self.last_edit_kind = Some(kind);
        self.cursor_char
    }

    pub fn add_cursor_vertical(&mut self, delta: i32) {
        let states = self.cursor_states();
        let selected = if delta < 0 {
            states.iter().min_by_key(|state| state.cursor)
        } else {
            states.iter().max_by_key(|state| state.cursor)
        };
        let Some(selected) = selected.copied() else {
            return;
        };
        let (line, column) = self.line_col_at_char(selected.cursor);
        let target_line = (line as i32 + delta).clamp(1, self.line_count() as i32) as usize;
        let target = self.char_index_for_line_col(target_line, column);
        self.add_cursor(CursorState::collapsed(target));
    }

    pub fn move_all_vertical(&mut self, delta: i32, extend: bool) {
        let states = self.cursor_states();
        let new_states: Vec<CursorState> = states
            .into_iter()
            .map(|state| {
                let (line, column) = self.line_col_at_char(state.cursor);
                let target_line = (line as i32 + delta).clamp(1, self.line_count() as i32) as usize;
                let target = self.char_index_for_line_col(target_line, column);
                CursorState::new(target, if extend { state.anchor } else { target })
            })
            .collect();
        self.set_cursor_states(new_states);
    }

    pub fn move_all_horizontal(&mut self, delta: i32, extend: bool) {
        let len = self.len_chars();
        let states = self.cursor_states();
        let new_states: Vec<CursorState> = states
            .into_iter()
            .map(|state| {
                let target = if delta < 0 {
                    state.cursor.saturating_sub((-delta) as usize)
                } else {
                    (state.cursor + delta as usize).min(len)
                };
                CursorState::new(target, if extend { state.anchor } else { target })
            })
            .collect();
        self.set_cursor_states(new_states);
    }

    pub fn move_all_home(&mut self, extend: bool) {
        let states = self.cursor_states();
        let new_states: Vec<CursorState> = states
            .into_iter()
            .map(|state| {
                let line = self.line_of_char(state.cursor);
                let target = self.line_start(line);
                CursorState::new(target, if extend { state.anchor } else { target })
            })
            .collect();
        self.set_cursor_states(new_states);
    }

    pub fn move_all_end(&mut self, extend: bool) {
        let states = self.cursor_states();
        let new_states: Vec<CursorState> = states
            .into_iter()
            .map(|state| {
                let line = self.line_of_char(state.cursor);
                let target = self.line_start(line) + self.line_text(line).chars().count();
                CursorState::new(target, if extend { state.anchor } else { target })
            })
            .collect();
        self.set_cursor_states(new_states);
    }

    pub fn undo(&mut self) -> bool {
        let Some(record) = self.undo.pop() else {
            return false;
        };
        let old_line_count = self.line_count();
        let mut final_starts = Vec::with_capacity(record.edits.len());
        let mut offset = 0_isize;
        for edit in &record.edits {
            final_starts.push((edit.start as isize + offset).max(0) as usize);
            offset +=
                edit.inserted.chars().count() as isize - edit.removed.chars().count() as isize;
        }
        for (edit, final_start) in record.edits.iter().zip(final_starts.iter()).rev() {
            let remove_len = edit.inserted.chars().count();
            if remove_len > 0 {
                self.rope.remove(*final_start..*final_start + remove_len);
            }
            if !edit.removed.is_empty() {
                self.rope.insert(*final_start, &edit.removed);
            }
        }
        self.set_cursor_states(record.cursors_before.clone());
        self.dirty = true;
        self.revision += 1;
        self.last_edit_line = self.rope.char_to_line(
            record
                .edits
                .first()
                .map(|edit| edit.start)
                .unwrap_or(0)
                .min(self.len_chars()),
        );
        self.last_edit_line_delta = self.line_count() as isize - old_line_count as isize;
        self.redo.push(record);
        self.last_edit_at = None;
        self.last_edit_kind = None;
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(record) = self.redo.pop() else {
            return false;
        };
        let old_line_count = self.line_count();
        for edit in record.edits.iter().rev() {
            let remove_len = edit.removed.chars().count();
            if remove_len > 0 {
                self.rope.remove(edit.start..edit.start + remove_len);
            }
            if !edit.inserted.is_empty() {
                self.rope.insert(edit.start, &edit.inserted);
            }
        }
        self.set_cursor_states(record.cursors_after.clone());
        self.dirty = true;
        self.revision += 1;
        self.last_edit_line = self.rope.char_to_line(
            record
                .edits
                .first()
                .map(|edit| edit.start)
                .unwrap_or(0)
                .min(self.len_chars()),
        );
        self.last_edit_line_delta = self.line_count() as isize - old_line_count as isize;
        self.undo.push(record);
        self.last_edit_at = None;
        self.last_edit_kind = None;
        true
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn save(&mut self) -> Result<()> {
        let file = File::create(&self.path)
            .with_context(|| format!("failed to create {}", self.path.display()))?;
        let mut writer = BufWriter::new(file);
        self.rope
            .write_to(&mut writer)
            .with_context(|| format!("failed to write {}", self.path.display()))?;
        writer
            .flush()
            .with_context(|| format!("failed to flush {}", self.path.display()))?;
        self.dirty = false;
        Ok(())
    }

    /// Reload from disk, preserving the cursor when possible.
    pub fn reload(&mut self) -> Result<()> {
        let fresh = Self::open(&self.path)?;
        self.rope = fresh.rope;
        self.cursor_char = self.cursor_char.min(self.len_chars());
        self.anchor_char = self.anchor_char.min(self.len_chars());
        self.extra_cursors.clear();
        self.dirty = false;
        self.revision += 1;
        self.last_edit_line = 0;
        self.last_edit_line_delta = 0;
        self.undo.clear();
        self.redo.clear();
        self.last_edit_at = None;
        self.last_edit_kind = None;
        Ok(())
    }

    /// Replace the buffer with recovered crash content and mark it dirty.
    pub fn restore_text(&mut self, text: &str, cursor: usize, anchor: usize) {
        self.rope = Rope::from_str(text);
        self.cursor_char = cursor.min(self.len_chars());
        self.anchor_char = anchor.min(self.len_chars());
        self.extra_cursors.clear();
        self.dirty = true;
        self.revision += 1;
        self.last_edit_line = 0;
        self.last_edit_line_delta = 0;
        self.undo.clear();
        self.redo.clear();
        self.last_edit_at = None;
        self.last_edit_kind = None;
    }
}

/// Convert a character index to a 1-based (line, column).
pub fn line_col_at_char(text: &str, char_index: usize) -> (usize, usize) {
    let mut line = 1;
    let mut column = 1;
    for (index, ch) in text.chars().enumerate() {
        if index >= char_index {
            break;
        }
        match ch {
            '\n' => {
                line += 1;
                column = 1;
            }
            '\r' => {}
            _ => column += 1,
        }
    }
    (line, column)
}

/// Convert a 1-based (line, column) to a character index, clamped to the line.
pub fn char_index_for_line_col(text: &str, line: usize, column: usize) -> usize {
    let line = line.max(1);
    let column = column.max(1);
    let mut line_starts = vec![0_usize];
    for (index, ch) in text.chars().enumerate() {
        if ch == '\n' {
            line_starts.push(index + 1);
        }
    }

    let Some(&start) = line_starts.get(line - 1) else {
        return text.chars().count();
    };
    let next = line_starts
        .get(line)
        .copied()
        .unwrap_or_else(|| text.chars().count());
    let line_len = text
        .chars()
        .skip(start)
        .take(next.saturating_sub(start))
        .filter(|c| *c != '\n' && *c != '\r')
        .count();
    start + (column - 1).min(line_len)
}

/// Returns the identifier under (or immediately before) the cursor.
pub fn identifier_at(text: &str, char_index: usize) -> Option<(String, usize, usize)> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return None;
    }
    let mut index = char_index.min(chars.len().saturating_sub(1));
    if !is_identifier_char(chars.get(index).copied()) {
        if index > 0 && is_identifier_char(chars.get(index - 1).copied()) {
            index -= 1;
        } else if index + 1 < chars.len() && is_identifier_char(chars.get(index + 1).copied()) {
            index += 1;
        } else {
            return None;
        }
    }

    let mut start = index;
    while start > 0 && is_identifier_char(chars.get(start - 1).copied()) {
        start -= 1;
    }
    let mut end = index + 1;
    while end < chars.len() && is_identifier_char(chars.get(end).copied()) {
        end += 1;
    }
    Some((chars[start..end].iter().collect(), start, end))
}

pub fn slice_chars(text: &str, start: usize, end: usize) -> String {
    text.chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect()
}

fn is_identifier_char(ch: Option<char>) -> bool {
    ch.map(|ch| ch.is_alphanumeric() || ch == '_' || ch == '$')
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> Document {
        Document {
            path: PathBuf::from("test.java"),
            rope: Rope::from_str(text),
            dirty: false,
            revision: 0,
            cursor_char: 0,
            anchor_char: 0,
            extra_cursors: Vec::new(),
            last_edit_line: 0,
            last_edit_line_delta: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            last_edit_at: None,
            last_edit_kind: None,
        }
    }

    #[test]
    fn line_and_column_round_trip() {
        let text = "class A {\n  void run() {}\n}\n";
        assert_eq!(line_col_at_char(text, 0), (1, 1));
        assert_eq!(line_col_at_char(text, 10), (2, 1));
        assert_eq!(line_col_at_char(text, 12), (2, 3));
        assert_eq!(char_index_for_line_col(text, 2, 3), 12);
        assert_eq!(char_index_for_line_col(text, 99, 99), 28);

        let document = doc(text);
        assert_eq!(document.line_col_at_char(12), (2, 3));
        assert_eq!(document.char_index_for_line_col(2, 3), 12);
        assert_eq!(document.line_count(), 4);
    }

    #[test]
    fn finds_identifier_under_cursor() {
        let text = "class Hello { void run() {} }";
        let (word, start, end) = identifier_at(text, 8).unwrap();
        assert_eq!(word, "Hello");
        assert_eq!((start, end), (6, 11));
        let (word, _, _) = identifier_at(text, 11).unwrap();
        assert_eq!(word, "Hello");
    }

    #[test]
    fn edits_are_undoable_and_redoable() {
        let mut document = doc("hello");
        document.set_cursor(5, false);
        document.edit(5, 5, " world");
        assert_eq!(document.text(), "hello world");
        assert!(document.can_undo());
        assert!(document.undo());
        assert_eq!(document.text(), "hello");
        assert!(document.can_redo());
        assert!(document.redo());
        assert_eq!(document.text(), "hello world");
    }

    #[test]
    fn undo_groups_consecutive_typing() {
        let mut document = doc("");
        document.edit(0, 0, "a");
        document.edit(1, 1, "b");
        document.edit(2, 2, "c");
        assert_eq!(document.text(), "abc");
        assert!(document.undo());
        assert_eq!(document.text(), "");
    }

    #[test]
    fn selection_edit_replaces_range() {
        let mut document = doc("one two three");
        document.set_selection(4, 7);
        document.insert("2");
        assert_eq!(document.text(), "one 2 three");
        assert_eq!(document.cursor_char, 5);
        assert_eq!(document.selection(), None);
    }

    #[test]
    fn multi_cursor_insert_and_undo() {
        let mut document = doc("abc\ndef");
        document.set_cursor_states(vec![CursorState::collapsed(0), CursorState::collapsed(4)]);
        document.insert("X");
        assert_eq!(document.text(), "Xabc\nXdef");
        assert_eq!(document.cursor_states().len(), 2);
        assert!(document.undo());
        assert_eq!(document.text(), "abc\ndef");
        assert_eq!(document.cursor_states().len(), 2);
    }

    #[test]
    fn column_selection_replaces_each_line() {
        let mut document = doc("aa\nbb\ncc");
        document.set_multi_selection(vec![(0, 1), (3, 4), (6, 7)]);
        document.insert("Z");
        assert_eq!(document.text(), "Za\nZb\nZc");
        assert!(document.undo());
        assert_eq!(document.text(), "aa\nbb\ncc");
    }

    #[test]
    fn backspace_with_multiple_cursors() {
        let mut document = doc("aa\nbb");
        document.set_cursor_states(vec![CursorState::collapsed(2), CursorState::collapsed(5)]);
        assert!(document.backspace());
        assert_eq!(document.text(), "a\nb");
    }

    #[test]
    fn adds_cursors_below_the_lowest_cursor() {
        let mut document = doc("a\nb\nc\n");
        document.set_cursor(0, false);
        document.add_cursor_vertical(1);
        document.add_cursor_vertical(1);
        assert_eq!(document.cursor_states().len(), 3);
    }

    #[test]
    fn multi_cursor_typing_groups_undo() {
        let mut document = doc("abc\ndef");
        document.set_cursor_states(vec![CursorState::collapsed(0), CursorState::collapsed(4)]);
        document.insert("X");
        document.insert("Y");
        document.insert("Z");
        assert_eq!(document.text(), "XYZabc\nXYZdef");
        assert!(document.undo());
        assert_eq!(document.text(), "abc\ndef");
        assert_eq!(document.cursor_states().len(), 2);
    }

    #[test]
    fn slices_by_character_index() {
        assert_eq!(slice_chars("ação", 1, 3), "çã");
    }
}
