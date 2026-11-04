//! Incremental project index built from the syntax layer.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use rayon::prelude::*;
use reclipse_core::Project;
use tree_sitter::Parser;

#[cfg(test)]
use crate::parse::SymbolKind;
use crate::parse::{parse_source, parse_source_with, FileSymbols, JavaSymbol};

#[derive(Debug, Clone)]
pub struct SymbolLocation {
    pub file: PathBuf,
    pub symbol: JavaSymbol,
}

#[derive(Debug, Clone)]
pub struct IndexedProblem {
    pub file: PathBuf,
    pub line: usize,
    pub column: usize,
    pub message: String,
}

#[derive(Debug, Clone, Default)]
pub struct IndexStats {
    pub java_files: usize,
    pub symbols: usize,
    pub problems: usize,
    pub elapsed_ms: u128,
}

#[derive(Debug, Clone)]
pub struct ProjectIndex {
    pub project: Project,
    pub files: BTreeMap<PathBuf, FileSymbols>,
    pub by_name: HashMap<String, Vec<SymbolLocation>>,
    pub problems: Vec<IndexedProblem>,
    pub stats: IndexStats,
}

impl ProjectIndex {
    pub fn from_files(project: Project, files: Vec<FileSymbols>, elapsed_ms: u128) -> Self {
        let mut map = BTreeMap::new();
        for file in files {
            map.insert(file.path.clone(), file);
        }
        let mut index = Self {
            project,
            files: map,
            by_name: HashMap::new(),
            problems: Vec::new(),
            stats: IndexStats {
                elapsed_ms,
                ..Default::default()
            },
        };
        index.recompute_derived();
        index
    }

    /// Parse one file on the current thread and update the index.
    pub fn update_file(&mut self, path: &Path, source: &str) {
        let parsed = parse_source(path, source);
        self.files.insert(path.to_path_buf(), parsed);
        self.recompute_derived();
    }

    pub fn remove_file(&mut self, path: &Path) {
        if self.files.remove(path).is_some() {
            self.recompute_derived();
        }
    }

    pub fn symbols_for(&self, path: &Path) -> Option<&FileSymbols> {
        self.files.get(path)
    }

    pub fn find_symbols(&self, name: &str) -> Option<&Vec<SymbolLocation>> {
        self.by_name.get(name)
    }

    /// Pick the most useful definition: same file first, then types, then the
    /// first symbol in path order.
    pub fn find_best(&self, name: &str, from_file: &Path) -> Option<&SymbolLocation> {
        let candidates = self.by_name.get(name)?;
        candidates.iter().max_by_key(|location| {
            let mut score = 0_i32;
            if location.file == from_file {
                score += 100;
            }
            if location.symbol.kind.is_type() {
                score += 10;
            }
            if location.symbol.name == name {
                score += 5;
            }
            score
        })
    }

    fn recompute_derived(&mut self) {
        self.by_name.clear();
        self.problems.clear();
        let mut symbol_count = 0;

        for file in self.files.values() {
            if let Some(error) = &file.parse_error {
                self.problems.push(IndexedProblem {
                    file: file.path.clone(),
                    line: 1,
                    column: 1,
                    message: error.clone(),
                });
            }
            for error in &file.errors {
                self.problems.push(IndexedProblem {
                    file: file.path.clone(),
                    line: error.line,
                    column: error.column,
                    message: error.message.clone(),
                });
            }
            for symbol in &file.symbols {
                symbol_count += 1;
                self.by_name
                    .entry(symbol.name.clone())
                    .or_default()
                    .push(SymbolLocation {
                        file: file.path.clone(),
                        symbol: symbol.clone(),
                    });
            }
        }

        self.problems.sort_by(|a, b| {
            a.file
                .cmp(&b.file)
                .then(a.line.cmp(&b.line))
                .then(a.column.cmp(&b.column))
        });

        self.stats.java_files = self.files.len();
        self.stats.symbols = symbol_count;
        self.stats.problems = self.problems.len();
    }
}

/// Index all Java files in parallel.
///
/// `progress` is called from multiple worker threads and may be invoked more
/// than once for the same count.
pub fn build_index(
    project: Project,
    progress: Option<&(dyn Fn(usize, usize) + Sync)>,
) -> ProjectIndex {
    let start = Instant::now();
    let total = project.java_files.len();
    let done = AtomicUsize::new(0);

    let files: Vec<FileSymbols> = project
        .java_files
        .par_iter()
        .map_init(Parser::new, |parser, path| {
            let parsed = parse_file(parser, path);
            let current = done.fetch_add(1, Ordering::Relaxed) + 1;
            if let Some(progress) = progress {
                progress(current, total);
            }
            parsed
        })
        .collect();

    ProjectIndex::from_files(project, files, start.elapsed().as_millis())
}

fn parse_file(parser: &mut Parser, path: &Path) -> FileSymbols {
    match fs::read_to_string(path) {
        Ok(source) => parse_source_with(parser, path, &source),
        Err(error) => {
            let mut file = FileSymbols::empty(path);
            file.parse_error = Some(format!("could not read file: {error}"));
            file
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn builds_symbol_table_and_updates_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/App.java"),
            "package demo; public class App { void run() {} }",
        )
        .unwrap();
        fs::write(
            root.join("src/Other.java"),
            "package demo; class Other { App app; }",
        )
        .unwrap();

        let project = reclipse_core::discover_project(root).unwrap();
        let mut index = build_index(project, None);
        assert_eq!(index.stats.java_files, 2);
        assert!(index.find_symbols("App").is_some());
        assert_eq!(
            index
                .find_best("App", &root.join("src/Other.java"))
                .unwrap()
                .symbol
                .kind,
            SymbolKind::Class
        );

        let app_path = index
            .files
            .keys()
            .find(|p| p.ends_with("App.java"))
            .unwrap()
            .clone();
        index.update_file(
            &app_path,
            "package demo; public class App { void start() {} }",
        );
        assert!(index.find_symbols("start").is_some());
        assert!(index.find_symbols("run").is_none());
    }
}
