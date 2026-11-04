//! Lightweight semantic queries over the source index and classpath index.
//!
//! This is the M2 foundation: import/type resolution, unresolved import
//! diagnostics and type discovery. Full type inference and member completion
//! build on this layer.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use reclipse_classpath::ClasspathIndex;

use crate::{FileSymbols, JavaSymbol, ProjectIndex, SymbolKind};

#[derive(Debug, Clone)]
pub struct ProjectTypeLocation {
    pub path: PathBuf,
    pub line: usize,
    pub column: usize,
    pub fqn: String,
    pub kind: SymbolKind,
}

#[derive(Debug, Clone, Default)]
pub struct ProjectTypes {
    pub by_fqn: HashMap<String, ProjectTypeLocation>,
    pub by_simple: HashMap<String, Vec<String>>,
    pub packages: HashSet<String>,
}

impl ProjectTypes {
    pub fn build(index: &ProjectIndex) -> Self {
        let mut types = Self::default();
        for (path, file) in &index.files {
            if let Some(package) = &file.package {
                types.packages.insert(package.clone());
            }
            for symbol in &file.symbols {
                if !symbol.kind.is_type() {
                    continue;
                }
                let fqn = type_fqn(file, symbol);
                let location = ProjectTypeLocation {
                    path: path.clone(),
                    line: symbol.line,
                    column: symbol.column,
                    fqn: fqn.clone(),
                    kind: symbol.kind,
                };
                types.by_fqn.entry(fqn.clone()).or_insert(location);
                types
                    .by_simple
                    .entry(symbol.name.clone())
                    .or_default()
                    .push(fqn);
            }
        }
        for names in types.by_simple.values_mut() {
            names.sort();
            names.dedup();
        }
        types
    }

    pub fn get(&self, fqn: &str) -> Option<&ProjectTypeLocation> {
        self.by_fqn.get(fqn)
    }
}

#[derive(Debug, Clone)]
pub enum ResolvedType {
    Project(ProjectTypeLocation),
    Classpath(String),
}

impl ResolvedType {
    pub fn name(&self) -> &str {
        match self {
            Self::Project(location) => &location.fqn,
            Self::Classpath(name) => name,
        }
    }
}

pub fn type_fqn(file: &FileSymbols, symbol: &JavaSymbol) -> String {
    let mut parts = Vec::new();
    if let Some(package) = &file.package {
        if !package.is_empty() {
            parts.push(package.clone());
        }
    }
    if let Some(container) = &symbol.container {
        if !container.is_empty() {
            parts.push(container.clone());
        }
    }
    parts.push(symbol.name.clone());
    parts.join(".")
}

/// Resolve a simple or qualified type name in the context of a source file.
pub fn resolve_type(
    name: &str,
    file: &FileSymbols,
    project: &ProjectTypes,
    classpath: &ClasspathIndex,
) -> Option<ResolvedType> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }

    if name.contains('.') {
        if let Some(found) = resolve_fqn(name, project, classpath) {
            return Some(found);
        }
    }

    // Explicit imports.
    for import in &file.imports {
        let import = import.trim();
        if import.ends_with('*') {
            continue;
        }
        let mut path = import.strip_prefix("static ").unwrap_or(import).trim();
        if path.is_empty() {
            continue;
        }
        if import.starts_with("static ") {
            if let Some((owner, _member)) = path.rsplit_once('.') {
                path = owner;
            }
        }
        let last = path.rsplit('.').next().unwrap_or(path);
        let normalized = last.replace('$', ".");
        let last = normalized
            .rsplit('.')
            .next()
            .unwrap_or(normalized.as_str())
            .to_owned();
        if last == name {
            if let Some(found) = resolve_fqn(path, project, classpath) {
                return Some(found);
            }
        }
    }

    // Same package.
    if let Some(package) = &file.package {
        let candidate = format!("{package}.{name}");
        if let Some(found) = resolve_fqn(&candidate, project, classpath) {
            return Some(found);
        }
    }

    // Wildcard imports.
    for import in &file.imports {
        let import = import.trim();
        if let Some(package) = import.strip_suffix(".*") {
            let candidate = format!("{package}.{name}");
            if let Some(found) = resolve_fqn(&candidate, project, classpath) {
                return Some(found);
            }
        }
    }

    // java.lang is implicit.
    let candidate = format!("java.lang.{name}");
    if let Some(found) = resolve_fqn(&candidate, project, classpath) {
        return Some(found);
    }

    // Unique project/classpath simple name.
    if let Some(names) = project.by_simple.get(name) {
        if names.len() == 1 {
            if let Some(location) = project.get(&names[0]) {
                return Some(ResolvedType::Project(location.clone()));
            }
        }
    }
    let matches = classpath.simple_matches(name);
    if matches.len() == 1 {
        return Some(ResolvedType::Classpath(matches[0].clone()));
    }

    None
}

fn resolve_fqn(
    fqn: &str,
    project: &ProjectTypes,
    classpath: &ClasspathIndex,
) -> Option<ResolvedType> {
    if let Some(location) = project.get(fqn) {
        return Some(ResolvedType::Project(location.clone()));
    }
    if let Some(class) = classpath.get_loose(fqn) {
        return Some(ResolvedType::Classpath(class.name.clone()));
    }
    None
}

pub fn import_type_path(import: &str) -> Option<String> {
    let import = import.trim();
    if import.is_empty() || import.ends_with(".*") {
        return None;
    }
    let mut path = import.strip_prefix("static ").unwrap_or(import).trim();
    if import.starts_with("static ") {
        if let Some((owner, _member)) = path.rsplit_once('.') {
            path = owner;
        }
    }
    Some(path.to_owned())
}

pub fn unresolved_imports(
    file: &FileSymbols,
    project: &ProjectTypes,
    classpath: &ClasspathIndex,
) -> Vec<(String, usize)> {
    let mut unresolved = Vec::new();
    for (index, import) in file.imports.iter().enumerate() {
        let line = file.import_lines.get(index).copied().unwrap_or(1);
        let import = import.trim();
        if let Some(package) = import.strip_suffix(".*") {
            let package = package.strip_prefix("static ").unwrap_or(package);
            if !project.packages.contains(package) && !classpath.package_exists(package) {
                unresolved.push((import.to_owned(), line));
            }
            continue;
        }
        let Some(path) = import_type_path(import) else {
            continue;
        };
        if project.get(&path).is_none() && classpath.get_loose(&path).is_none() {
            unresolved.push((import.to_owned(), line));
        }
    }
    unresolved
}

/// Type names available for completion, with imports and java.lang first.
pub fn completion_type_names(
    file: &FileSymbols,
    prefix: &str,
    project: &ProjectTypes,
    classpath: &ClasspathIndex,
) -> Vec<String> {
    let prefix_lower = prefix.to_lowercase();
    let mut names = Vec::new();
    let mut push = |name: String| {
        if prefix.is_empty() || name.to_lowercase().contains(&prefix_lower) {
            names.push(name);
        }
    };

    for import in &file.imports {
        if let Some(path) = import_type_path(import) {
            push(path);
        }
    }
    for package in ["java.lang"] {
        for name in project
            .by_fqn
            .keys()
            .filter(|name| name.starts_with(package))
        {
            push(name.clone());
        }
    }
    for name in project.by_fqn.keys() {
        push(name.clone());
    }
    for name in &classpath.type_names {
        push(name.clone());
    }
    names.sort();
    names.dedup();
    names.truncate(500);
    names
}

/// Try to infer the declared type of a local variable/field near the cursor.
/// This intentionally handles simple declarations (`Type name = ...`) only.
pub fn infer_local_type(source: &str, char_index: usize, variable: &str) -> Option<String> {
    let before: String = source.chars().take(char_index).collect();
    let line = before.rsplit('\n').next().unwrap_or(&before);
    let words: Vec<&str> = line.split_whitespace().collect();
    let position = words.iter().position(|word| *word == variable)?;
    if position == 0 {
        return None;
    }
    let mut candidate = words[position - 1].trim();
    if let Some(generic) = candidate.find('<') {
        candidate = &candidate[..generic];
    }
    candidate = candidate.trim_end_matches("[]");
    let candidate = candidate.rsplit('.').next().unwrap_or(candidate);
    if candidate
        .chars()
        .next()
        .is_some_and(|character| character.is_uppercase())
    {
        Some(candidate.to_owned())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_project_type_fqn() {
        let file = FileSymbols {
            path: PathBuf::from("A.java"),
            package: Some("com.acme".to_owned()),
            imports: Vec::new(),
            import_lines: Vec::new(),
            symbols: Vec::new(),
            errors: Vec::new(),
            line_count: 1,
            parse_error: None,
        };
        let symbol = JavaSymbol {
            name: "Inner".to_owned(),
            kind: SymbolKind::Class,
            line: 3,
            column: 1,
            container: Some("Outer".to_owned()),
            depth: 1,
            detail: None,
        };
        assert_eq!(type_fqn(&file, &symbol), "com.acme.Outer.Inner");
    }

    #[test]
    fn resolves_imported_project_type() {
        let mut project = ProjectTypes::default();
        project.by_fqn.insert(
            "com.acme.User".to_owned(),
            ProjectTypeLocation {
                path: PathBuf::from("User.java"),
                line: 1,
                column: 1,
                fqn: "com.acme.User".to_owned(),
                kind: SymbolKind::Class,
            },
        );
        let file = FileSymbols {
            path: PathBuf::from("App.java"),
            package: Some("com.acme".to_owned()),
            imports: vec!["com.acme.User".to_owned()],
            import_lines: vec![3],
            symbols: Vec::new(),
            errors: Vec::new(),
            line_count: 1,
            parse_error: None,
        };
        let classpath = ClasspathIndex::default();
        assert!(unresolved_imports(&file, &project, &classpath).is_empty());
        assert!(matches!(
            resolve_type("User", &file, &project, &classpath),
            Some(ResolvedType::Project(_))
        ));
    }

    #[test]
    fn reports_unresolved_import_with_line() {
        let project = ProjectTypes::default();
        let file = FileSymbols {
            path: PathBuf::from("App.java"),
            package: Some("com.acme".to_owned()),
            imports: vec!["java.util.List".to_owned()],
            import_lines: vec![4],
            symbols: Vec::new(),
            errors: Vec::new(),
            line_count: 1,
            parse_error: None,
        };
        let unresolved = unresolved_imports(&file, &project, &ClasspathIndex::default());
        assert_eq!(unresolved, vec![("java.util.List".to_owned(), 4)]);
    }
}
