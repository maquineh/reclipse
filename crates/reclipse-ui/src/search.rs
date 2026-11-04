//! In-file and project-wide literal search state.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use reclipse_core::Project;
use reclipse_editor::{Document, SearchMatcher, SearchOptions};

#[derive(Debug, Default)]
pub struct FileSearch {
    pub open: bool,
    pub replace: bool,
    pub query: String,
    pub replacement: String,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
    pub matches: Vec<(usize, usize)>,
    pub current: usize,
    pub focus_pending: bool,
    pub error: Option<String>,
    last_query: String,
    last_revision: u64,
    last_case: bool,
    last_whole_word: bool,
    last_regex: bool,
}

impl FileSearch {
    pub fn open(&mut self, replace: bool) {
        self.open = true;
        self.replace = replace;
        self.focus_pending = true;
        self.last_query.clear();
        self.last_revision = u64::MAX;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.matches.clear();
        self.error = None;
        self.current = 0;
        self.focus_pending = false;
    }

    pub fn ensure(&mut self, doc: &Document) {
        let changed = self.last_query != self.query
            || self.last_revision != doc.revision
            || self.last_case != self.case_sensitive
            || self.last_whole_word != self.whole_word
            || self.last_regex != self.regex;
        if !changed {
            return;
        }

        let query_changed = self.last_query != self.query
            || self.last_case != self.case_sensitive
            || self.last_whole_word != self.whole_word
            || self.last_regex != self.regex;
        let options = SearchOptions {
            case_sensitive: self.case_sensitive,
            whole_word: self.whole_word,
            regex: self.regex,
        };
        match SearchMatcher::new(&self.query, options) {
            Ok(matcher) => {
                self.matches = matcher.find_all(&doc.text());
                self.error = None;
            }
            Err(error) => {
                self.matches.clear();
                self.error = Some(error);
            }
        }
        if query_changed {
            self.current = 0;
        } else {
            self.current = self.current.min(self.matches.len().saturating_sub(1));
        }
        self.last_query = self.query.clone();
        self.last_revision = doc.revision;
        self.last_case = self.case_sensitive;
        self.last_whole_word = self.whole_word;
        self.last_regex = self.regex;
    }

    pub fn current_match(&self) -> Option<(usize, usize)> {
        self.matches.get(self.current).copied()
    }

    pub fn next(&mut self) {
        if !self.matches.is_empty() {
            self.current = (self.current + 1) % self.matches.len();
        }
    }

    pub fn previous(&mut self) {
        if !self.matches.is_empty() {
            self.current = if self.current == 0 {
                self.matches.len() - 1
            } else {
                self.current - 1
            };
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProjectMatch {
    pub file: PathBuf,
    pub line: usize,
    pub column: usize,
    pub start: usize,
    pub end: usize,
    pub preview: String,
}

#[derive(Debug, Default)]
pub struct ProjectSearch {
    pub open: bool,
    pub query: String,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
    pub results: Vec<ProjectMatch>,
    pub selected: usize,
    pub running: bool,
    pub searched: bool,
    pub error: Option<String>,
    pub replacement: String,
    pub focus_pending: bool,
    rx: Option<Receiver<Vec<ProjectMatch>>>,
}

impl ProjectSearch {
    pub fn open(&mut self) {
        self.open = true;
        self.focus_pending = true;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.running = false;
        self.rx = None;
        self.focus_pending = false;
    }

    pub fn start(&mut self, files: Vec<PathBuf>) {
        let query = self.query.trim().to_owned();
        self.searched = true;
        self.error = None;
        self.selected = 0;
        if query.is_empty() {
            self.results.clear();
            self.running = false;
            self.rx = None;
            return;
        }

        let options = SearchOptions {
            case_sensitive: self.case_sensitive,
            whole_word: self.whole_word,
            regex: self.regex,
        };
        let matcher = match SearchMatcher::new(&query, options) {
            Ok(matcher) => matcher,
            Err(error) => {
                self.results.clear();
                self.running = false;
                self.rx = None;
                self.error = Some(error);
                return;
            }
        };

        let (sender, receiver) = mpsc::channel();
        self.rx = Some(receiver);
        self.running = true;

        thread::spawn(move || {
            let results = search_files(files, matcher);
            let _ = sender.send(results);
        });
    }

    pub fn poll(&mut self) -> bool {
        let Some(receiver) = &self.rx else {
            return false;
        };
        match receiver.try_recv() {
            Ok(results) => {
                self.results = results;
                self.selected = 0;
                self.running = false;
                self.rx = None;
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.running = false;
                self.rx = None;
                self.error = Some("a busca terminou inesperadamente".to_owned());
                true
            }
        }
    }

    pub fn next(&mut self) {
        if !self.results.is_empty() {
            self.selected = (self.selected + 1) % self.results.len();
        }
    }

    pub fn previous(&mut self) {
        if !self.results.is_empty() {
            self.selected = if self.selected == 0 {
                self.results.len() - 1
            } else {
                self.selected - 1
            };
        }
    }
}

fn search_files(files: Vec<PathBuf>, matcher: SearchMatcher) -> Vec<ProjectMatch> {
    const MAX_RESULTS: usize = 2_000;
    const MAX_PER_FILE: usize = 100;
    const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

    let mut results = Vec::new();
    for file in files {
        if results.len() >= MAX_RESULTS {
            break;
        }
        if !is_searchable(&file) {
            continue;
        }
        if std::fs::metadata(&file)
            .map(|metadata| metadata.len() > MAX_FILE_BYTES)
            .unwrap_or(true)
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let matches = matcher.find_all(&text);
        if matches.is_empty() {
            continue;
        }

        let line_starts = line_starts(&text);
        let lines: Vec<&str> = text.lines().collect();
        for (start, end) in matches.into_iter().take(MAX_PER_FILE) {
            let line_index = line_starts
                .partition_point(|line_start| *line_start <= start)
                .saturating_sub(1);
            let line_start = line_starts.get(line_index).copied().unwrap_or(0);
            let preview = lines
                .get(line_index)
                .copied()
                .unwrap_or("")
                .trim()
                .chars()
                .take(160)
                .collect();
            results.push(ProjectMatch {
                file: file.clone(),
                line: line_index + 1,
                column: start - line_start + 1,
                start,
                end,
                preview,
            });
            if results.len() >= MAX_RESULTS {
                break;
            }
        }
    }
    results
}

fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (index, character) in text.chars().enumerate() {
        if character == '\n' {
            starts.push(index + 1);
        }
    }
    starts
}

fn is_searchable(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "java"
                    | "kt"
                    | "kts"
                    | "xml"
                    | "properties"
                    | "md"
                    | "json"
                    | "yml"
                    | "yaml"
                    | "toml"
                    | "gradle"
                    | "txt"
                    | "sh"
                    | "bat"
                    | "ps1"
                    | "rs"
                    | "html"
                    | "css"
                    | "js"
                    | "ts"
            )
        })
        .unwrap_or(false)
}

/// Convenience for the app: search all text-like files of a project.
pub fn project_files(project: &Project) -> Vec<PathBuf> {
    project
        .all_files
        .iter()
        .filter(|path| is_searchable(path))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_search_finds_lines() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("App.java");
        std::fs::write(&file, "class App {\n  // needle\n}\n").unwrap();
        let options = SearchOptions {
            case_sensitive: true,
            whole_word: false,
            regex: false,
        };
        let matcher = SearchMatcher::new("needle", options).unwrap();
        let results = search_files(vec![file], matcher);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].line, 2);
        assert_eq!(results[0].column, 6);
    }

    #[test]
    fn project_search_supports_regex() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("App.java");
        std::fs::write(&file, "class App {\n  void run() {}\n}\n").unwrap();
        let options = SearchOptions {
            case_sensitive: true,
            whole_word: false,
            regex: true,
        };
        let matcher = SearchMatcher::new(r"void\s+\w+", options).unwrap();
        let results = search_files(vec![file], matcher);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].line, 2);
    }

    #[test]
    fn line_starts_use_character_offsets() {
        let starts = line_starts("ação\nx");
        assert_eq!(starts, vec![0, 5]);
    }
}
