//! Command palette / quick-open model.

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub enum Action {
    OpenFolder,
    QuickOpen,
    OpenFile,
    Save,
    CloseTab,
    Reindex,
    ToggleProject,
    ToggleOutline,
    ToggleProblems,
    GoToDefinition,
    FindInFile,
    ReplaceInFile,
    FindInProject,
    FindNext,
    FindPrevious,
    Undo,
    Redo,
    AddCursorBelow,
    AddCursorAbove,
    ClearExtraCursors,
    OpenSettings,
    ZoomIn,
    ZoomOut,
    ReloadPlugins,
    About,
    OpenPath(PathBuf),
    OpenAt(PathBuf, usize, usize),
    OpenClasspathType(String),
    OpenTypes,
    QuickOutline,
    ShowShortcuts,
    Plugin {
        plugin_index: usize,
        command_id: String,
        title: String,
    },
}

#[derive(Debug, Clone)]
pub struct PaletteEntry {
    pub label: String,
    pub hint: String,
    pub action: Action,
}

impl PaletteEntry {
    pub fn new(label: impl Into<String>, hint: impl Into<String>, action: Action) -> Self {
        Self {
            label: label.into(),
            hint: hint.into(),
            action,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteMode {
    Commands,
    Files,
    Types,
    Outline,
}

#[derive(Debug, Default)]
pub struct Palette {
    pub open: bool,
    pub mode: Option<PaletteMode>,
    pub query: String,
    pub selected: usize,
    pub focus_pending: bool,
}

impl Palette {
    pub fn open(&mut self, mode: PaletteMode) {
        self.open = true;
        self.mode = Some(mode);
        self.query.clear();
        self.selected = 0;
        self.focus_pending = true;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.mode = None;
        self.query.clear();
        self.selected = 0;
        self.focus_pending = false;
    }
}

/// Rank entries by a subsequence fuzzy score. Empty queries keep the provided
/// order (which is alphabetical for files, curated for commands).
pub fn rank(entries: Vec<PaletteEntry>, query: &str, limit: usize) -> Vec<PaletteEntry> {
    let query = query.trim();
    if query.is_empty() {
        return entries.into_iter().take(limit).collect();
    }

    let mut scored: Vec<(i32, PaletteEntry)> = entries
        .into_iter()
        .filter_map(|entry| {
            fuzzy_score(query, &entry.label)
                .or_else(|| fuzzy_score(query, &entry.hint))
                .map(|score| (score, entry))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.label.cmp(&b.1.label)));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, entry)| entry)
        .collect()
}

/// Returns a higher score for earlier, consecutive and word-boundary matches.
pub fn fuzzy_score(needle: &str, haystack: &str) -> Option<i32> {
    let needle = needle.to_lowercase();
    let haystack = haystack.to_lowercase();
    if needle.is_empty() {
        return Some(0);
    }
    let hay: Vec<char> = haystack.chars().collect();
    let mut position = 0;
    let mut score = 0_i32;
    let mut previous: Option<usize> = None;

    for character in needle.chars() {
        let found = hay
            .iter()
            .enumerate()
            .skip(position)
            .find(|(_, candidate)| **candidate == character)
            .map(|(index, _)| index);
        let index = found?;
        score += 10;
        if let Some(previous) = previous {
            if index == previous + 1 {
                score += 20;
            } else {
                score -= 2 * (index - previous - 1) as i32;
            }
        }
        if index == 0 || matches!(hay[index - 1], '/' | '\\' | '_' | '-' | '.' | ' ' | ':') {
            score += 8;
        }
        previous = Some(index);
        position = index + 1;
    }

    score -= (hay.len() / 20) as i32;
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_prefers_prefix_and_consecutive_matches() {
        let prefix = fuzzy_score("app", "App.java").unwrap();
        let scattered = fuzzy_score("app", "a_p_p.java").unwrap();
        assert!(prefix > scattered);
        assert!(fuzzy_score("xyz", "App.java").is_none());
    }

    #[test]
    fn ranks_commands_by_query() {
        let entries = vec![
            PaletteEntry::new("Abrir pasta", "Ctrl+Shift+O", Action::OpenFolder),
            PaletteEntry::new("Salvar", "Ctrl+S", Action::Save),
        ];
        let ranked = rank(entries, "salv", 10);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].label, "Salvar");
    }
}
