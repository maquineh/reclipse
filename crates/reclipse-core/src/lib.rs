//! Project discovery and workspace model for Reclipse.
//!
//! This crate intentionally has no UI and no Java parser dependency. It answers
//! one question: "what should the IDE index?".

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use walkdir::{DirEntry, WalkDir};

/// Directories that never contain user source code worth indexing.
pub const IGNORED_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".idea",
    ".settings",
    ".metadata",
    ".gradle",
    "node_modules",
    "target",
    "build",
    "out",
    "bin",
    "dist",
];

const MAX_FILES: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildSystem {
    Eclipse,
    Maven,
    Gradle,
    Plain,
}

impl BuildSystem {
    pub fn label(self) -> &'static str {
        match self {
            Self::Eclipse => "Eclipse",
            Self::Maven => "Maven",
            Self::Gradle => "Gradle",
            Self::Plain => "Plain",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Project {
    pub root: PathBuf,
    pub name: String,
    pub build_system: BuildSystem,
    pub source_roots: Vec<PathBuf>,
    pub java_files: Vec<PathBuf>,
    /// All regular files, used by quick-open. Capped at [`MAX_FILES`].
    pub all_files: Vec<PathBuf>,
}

impl Project {
    pub fn relative(&self, path: &Path) -> PathBuf {
        path.strip_prefix(&self.root)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| path.to_path_buf())
    }

    pub fn display_path(&self, path: &Path) -> String {
        self.relative(path).to_string_lossy().replace('\\', "/")
    }
}

/// Discover a project from a root directory. The returned paths are absolute
/// and canonicalized (without the Windows `\\?\` prefix).
pub fn discover_project(root: &Path) -> Result<Project> {
    if !root.exists() {
        bail!("project root does not exist: {}", root.display());
    }
    let root = dunce::canonicalize(root)
        .with_context(|| format!("failed to canonicalize {}", root.display()))?;
    if !root.is_dir() {
        bail!("project root is not a directory: {}", root.display());
    }

    let name = root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "project".to_owned());

    let build_system = detect_build_system(&root);
    let mut source_roots = Vec::new();

    if let Some(roots) = eclipse_source_roots(&root) {
        source_roots.extend(roots);
    }

    if source_roots.is_empty() {
        match build_system {
            BuildSystem::Maven | BuildSystem::Gradle => {
                for candidate in ["src/main/java", "src/test/java"] {
                    let path = root.join(candidate);
                    if path.is_dir() {
                        source_roots.push(path);
                    }
                }
            }
            _ => {}
        }
    }

    if source_roots.is_empty() {
        for candidate in ["src/main/java", "src/test/java", "src", "source", "java"] {
            let path = root.join(candidate);
            if path.is_dir() && contains_java(&path) {
                source_roots.push(path);
            }
        }
    }

    if source_roots.is_empty() {
        source_roots.push(root.clone());
    }

    dedup_existing_dirs(&mut source_roots);

    let mut java_files = Vec::new();
    for source_root in &source_roots {
        collect_java_files(source_root, &mut java_files);
    }
    dedup_sort(&mut java_files);

    let mut all_files = Vec::new();
    collect_all_files(&root, &mut all_files);
    dedup_sort(&mut all_files);

    Ok(Project {
        root,
        name,
        build_system,
        source_roots,
        java_files,
        all_files,
    })
}

fn detect_build_system(root: &Path) -> BuildSystem {
    if root.join(".classpath").is_file() || root.join(".project").is_file() {
        BuildSystem::Eclipse
    } else if root.join("pom.xml").is_file() {
        BuildSystem::Maven
    } else if root.join("build.gradle").is_file() || root.join("build.gradle.kts").is_file() {
        BuildSystem::Gradle
    } else {
        BuildSystem::Plain
    }
}

fn eclipse_source_roots(root: &Path) -> Option<Vec<PathBuf>> {
    let classpath = fs::read_to_string(root.join(".classpath")).ok()?;
    let mut roots = Vec::new();
    for line in classpath.lines() {
        if !line.contains("<classpathentry") {
            continue;
        }
        if xml_attr(line, "kind").as_deref() != Some("src") {
            continue;
        }
        let Some(raw_path) = xml_attr(line, "path") else {
            continue;
        };
        let raw_path = raw_path.trim();
        if raw_path.is_empty() || raw_path.starts_with('/') {
            // Workspace-relative entries such as `/OtherProject/src` cannot be
            // resolved from a single project folder; skip them instead of
            // indexing a sibling project by accident.
            continue;
        }

        let candidate = root.join(raw_path);
        if candidate.is_dir() {
            roots.push(candidate);
            continue;
        }

        // Sometimes .classpath contains `/ProjectName/src`. Try the part after
        // the project name as a best-effort fallback.
        if let Some((_, rest)) = raw_path.trim_start_matches('/').split_once('/') {
            let candidate = root.join(rest);
            if candidate.is_dir() {
                roots.push(candidate);
            }
        }
    }

    if roots.is_empty() {
        None
    } else {
        Some(roots)
    }
}

fn xml_attr(line: &str, name: &str) -> Option<String> {
    for quote in ['"', '\''] {
        let pattern = format!("{name}={quote}");
        if let Some(start) = line.find(&pattern) {
            let value_start = start + pattern.len();
            if let Some(len) = line[value_start..].find(quote) {
                return Some(line[value_start..value_start + len].to_owned());
            }
        }
    }
    None
}

fn contains_java(dir: &Path) -> bool {
    WalkDir::new(dir)
        .max_depth(6)
        .into_iter()
        .filter_entry(|e| !is_ignored(e))
        .filter_map(Result::ok)
        .any(|e| e.file_type().is_file() && is_java_file(e.path()))
}

fn collect_java_files(root: &Path, out: &mut Vec<PathBuf>) {
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !is_ignored(e))
        .filter_map(Result::ok)
    {
        if entry.file_type().is_file() && is_java_file(entry.path()) {
            out.push(entry.path().to_path_buf());
        }
    }
}

fn collect_all_files(root: &Path, out: &mut Vec<PathBuf>) {
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !is_ignored(e))
        .filter_map(Result::ok)
    {
        if entry.file_type().is_file() {
            out.push(entry.path().to_path_buf());
            if out.len() >= MAX_FILES {
                break;
            }
        }
    }
}

fn is_ignored(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return false;
    }
    entry.file_type().is_dir()
        && entry
            .file_name()
            .to_str()
            .map(|name| {
                IGNORED_DIRS
                    .iter()
                    .any(|ignored| name.eq_ignore_ascii_case(ignored))
            })
            .unwrap_or(false)
}

pub fn is_java_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("java"))
        .unwrap_or(false)
}

fn dedup_existing_dirs(dirs: &mut Vec<PathBuf>) {
    let mut seen = HashSet::new();
    dirs.retain(|dir| {
        dir.is_dir() && seen.insert(dunce::canonicalize(dir).unwrap_or_else(|_| dir.clone()))
    });
    for dir in dirs.iter_mut() {
        if let Ok(canonical) = dunce::canonicalize(&*dir) {
            *dir = canonical;
        }
    }
    dirs.sort();
    dirs.dedup();
}

fn dedup_sort(paths: &mut Vec<PathBuf>) {
    paths.sort();
    paths.dedup();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_maven_layout() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("pom.xml"), "<project/>").unwrap();
        let main = root.join("src/main/java/com/acme");
        let test = root.join("src/test/java/com/acme");
        fs::create_dir_all(&main).unwrap();
        fs::create_dir_all(&test).unwrap();
        fs::write(main.join("App.java"), "package com.acme; class App {}").unwrap();
        fs::write(
            test.join("AppTest.java"),
            "package com.acme; class AppTest {}",
        )
        .unwrap();
        fs::write(root.join("notes.txt"), "hello").unwrap();

        let project = discover_project(root).unwrap();
        assert_eq!(project.build_system, BuildSystem::Maven);
        assert_eq!(project.java_files.len(), 2);
        assert!(project.all_files.iter().any(|f| f.ends_with("notes.txt")));
        assert!(project
            .source_roots
            .iter()
            .any(|r| r.ends_with("src/main/java")));
    }

    #[test]
    fn discovers_eclipse_classpath() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let src = root.join("source/java");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            root.join(".classpath"),
            r#"<?xml version="1.0"?>
<classpath>
  <classpathentry kind="src" path="source/java"/>
  <classpathentry kind="con" path="org.eclipse.jdt.launching.JRE_CONTAINER"/>
  <classpathentry kind="output" path="bin"/>
</classpath>"#,
        )
        .unwrap();
        fs::write(src.join("A.java"), "class A {}").unwrap();

        let project = discover_project(root).unwrap();
        assert_eq!(project.build_system, BuildSystem::Eclipse);
        assert_eq!(
            project.source_roots,
            vec![dunce::canonicalize(src).unwrap()]
        );
        assert_eq!(project.java_files.len(), 1);
    }

    #[test]
    fn ignores_build_output() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/main/java")).unwrap();
        fs::create_dir_all(root.join("target/generated")).unwrap();
        fs::write(root.join("src/main/java/A.java"), "class A {}").unwrap();
        fs::write(root.join("target/generated/B.java"), "class B {}").unwrap();

        let project = discover_project(root).unwrap();
        assert!(project
            .java_files
            .iter()
            .all(|p| !p.to_string_lossy().contains("target")));
    }
}
