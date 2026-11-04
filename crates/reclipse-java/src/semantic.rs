//! Lightweight semantic queries over the source index and classpath index.
//!
//! This is the M2 foundation: import/type resolution, unresolved import
//! diagnostics and type discovery. Full type inference and member completion
//! build on this layer.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use reclipse_classpath::{field_type_name, method_return_type_name, ClasspathIndex};

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

#[derive(Debug, Clone)]
pub struct TypeReference {
    pub file: PathBuf,
    pub name: String,
    pub line: usize,
    pub column: usize,
    pub start_byte: usize,
    pub end_byte: usize,
}

/// Find syntactic type-identifier occurrences that resolve to `fqn`.
///
/// This is a semantic type reference index: identifier resolution uses imports,
/// package, wildcard and classpath rules, so comments/strings and unrelated
/// same-named types are not included.
pub fn find_type_references(
    fqn: &str,
    project: &ProjectTypes,
    index: &ProjectIndex,
    classpath: &ClasspathIndex,
) -> Vec<TypeReference> {
    let mut references = Vec::new();
    for (path, file) in &index.files {
        for reference in &file.type_refs {
            let Some(resolved) = resolve_type(&reference.name, file, project, classpath) else {
                continue;
            };
            if resolved.name() != fqn {
                continue;
            }
            references.push(TypeReference {
                file: path.clone(),
                name: reference.name.clone(),
                line: reference.line,
                column: reference.column,
                start_byte: reference.start_byte,
                end_byte: reference.end_byte,
            });
        }
    }
    references.sort_by(|left, right| {
        left.file
            .cmp(&right.file)
            .then(left.line.cmp(&right.line))
            .then(left.column.cmp(&right.column))
    });
    references
        .dedup_by(|left, right| left.file == right.file && left.start_byte == right.start_byte);
    references
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
    infer_variable_type(source, char_index, variable)
}

/// Infer a variable type by scanning backwards for its most recent declaration.
/// Handles fields, parameters and locals declared in previous lines.
pub fn infer_variable_type(source: &str, char_index: usize, variable: &str) -> Option<String> {
    if variable.is_empty() {
        return None;
    }
    let prefix: String = source.chars().take(char_index).collect();
    let mut search_end = prefix.len();
    while let Some(position) = prefix[..search_end].rfind(variable) {
        let end = position + variable.len();
        let boundary_left = prefix[..position].chars().next_back();
        let boundary_right = prefix[end..].chars().next();
        if !is_identifier_boundary(boundary_left) || !is_identifier_boundary(boundary_right) {
            search_end = position;
            continue;
        }
        let before = prefix[..position].trim_end();
        if before.ends_with('.') {
            search_end = position;
            continue;
        }
        if let Some(type_name) = type_before_text(before) {
            return Some(type_name);
        }
        if position == 0 {
            break;
        }
        search_end = position;
    }
    None
}

fn is_identifier_boundary(character: Option<char>) -> bool {
    character.is_none_or(|character| !is_identifier_char(character))
}

fn is_identifier_char(character: char) -> bool {
    character.is_alphanumeric() || character == '_' || character == '$'
}

fn type_before_text(text: &str) -> Option<String> {
    let mut text = text.trim_end();
    if text.ends_with(']') {
        let stripped = text.trim_end_matches(']').trim_end();
        let stripped = stripped.strip_suffix('[')?;
        text = stripped.trim_end();
    }
    if text.ends_with('>') {
        let open = matching_angle(text)?;
        text = text[..open].trim_end();
    }
    let start = text
        .char_indices()
        .rev()
        .take_while(|(_, character)| is_identifier_char(*character) || *character == '.')
        .last()
        .map(|(index, _)| index)?;
    let token = text[start..].trim_start_matches('.');
    let simple = token.rsplit('.').next()?;
    if simple.is_empty()
        || !simple
            .chars()
            .next()
            .is_some_and(|character| character.is_uppercase())
    {
        return None;
    }
    Some(simple.to_owned())
}

fn matching_angle(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (index, character) in text.char_indices().rev() {
        match character {
            '>' => depth += 1,
            '<' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    Type,
    Field,
    Method,
}

#[derive(Debug, Clone)]
pub struct CompletionItem {
    pub label: String,
    pub detail: String,
    pub kind: CompletionKind,
    pub insert_text: String,
}

#[derive(Debug, Clone)]
pub struct CompletionContext {
    /// Character index where the identifier being completed starts.
    pub start: usize,
    pub prefix: String,
    pub receiver: Option<String>,
    pub items: Vec<CompletionItem>,
}

/// Build completion items for the identifier at `cursor`.
pub fn completion_context(
    source: &str,
    cursor: usize,
    file: &FileSymbols,
    project: &ProjectTypes,
    project_index: Option<&ProjectIndex>,
    classpath: &ClasspathIndex,
) -> CompletionContext {
    let characters: Vec<char> = source.chars().collect();
    let cursor = cursor.min(characters.len());
    let mut start = cursor;
    while start > 0 && is_identifier_char(characters[start - 1]) {
        start -= 1;
    }
    let prefix: String = characters[start..cursor].iter().collect();
    let receiver = receiver_before(&characters, start);

    let items = if let Some(receiver) = &receiver {
        match resolve_receiver_type(
            receiver,
            source,
            cursor,
            file,
            project,
            project_index,
            classpath,
        ) {
            Some(resolved) => members_for_type(&resolved, project_index, classpath),
            None => package_type_items(receiver, &prefix, project, classpath),
        }
    } else {
        type_items(file, &prefix, project, classpath)
    };

    CompletionContext {
        start,
        prefix,
        receiver,
        items,
    }
}

fn receiver_before(characters: &[char], start: usize) -> Option<String> {
    let mut index = start;
    while index > 0 && characters[index - 1].is_whitespace() {
        index -= 1;
    }
    if index == 0 || characters[index - 1] != '.' {
        return None;
    }
    index -= 1;
    let end = index;
    while index > 0 && characters[index - 1].is_whitespace() {
        index -= 1;
    }
    let mut begin = index;
    while begin > 0 && (is_identifier_char(characters[begin - 1]) || characters[begin - 1] == '.') {
        begin -= 1;
    }
    let receiver: String = characters[begin..end].iter().collect();
    let receiver = receiver.trim_matches('.').to_owned();
    if receiver.is_empty() {
        None
    } else {
        Some(receiver)
    }
}

fn resolve_receiver_type(
    receiver: &str,
    source: &str,
    cursor: usize,
    file: &FileSymbols,
    project: &ProjectTypes,
    project_index: Option<&ProjectIndex>,
    classpath: &ClasspathIndex,
) -> Option<ResolvedType> {
    let segments: Vec<&str> = receiver
        .split('.')
        .filter(|part| !part.is_empty())
        .collect();
    let first = *segments.first()?;
    let mut current = if first == "this" {
        let fqn = active_type_fqn(file)?;
        resolve_fqn(&fqn, project, classpath)?
    } else {
        match resolve_type(first, file, project, classpath) {
            Some(resolved) => resolved,
            None => {
                let inferred = infer_variable_type(source, cursor, first)?;
                resolve_type(&inferred, file, project, classpath)?
            }
        }
    };
    for segment in &segments[1..] {
        current = member_type(&current, segment, project, project_index, classpath)?;
    }
    Some(current)
}

fn active_type_fqn(file: &FileSymbols) -> Option<String> {
    file.symbols
        .iter()
        .find(|symbol| symbol.kind.is_type())
        .map(|symbol| type_fqn(file, symbol))
}

fn member_type(
    receiver: &ResolvedType,
    member: &str,
    project: &ProjectTypes,
    project_index: Option<&ProjectIndex>,
    classpath: &ClasspathIndex,
) -> Option<ResolvedType> {
    match receiver {
        ResolvedType::Project(location) => {
            let index = project_index?;
            let file = index.files.get(&location.path)?;
            let container = type_container(file, location);
            let symbol = file.symbols.iter().find(|symbol| {
                symbol.name == member
                    && symbol.container.as_deref() == Some(container.as_str())
                    && matches!(
                        symbol.kind,
                        SymbolKind::Field | SymbolKind::Method | SymbolKind::EnumConstant
                    )
            })?;
            if symbol.kind == SymbolKind::EnumConstant {
                return resolve_fqn(&location.fqn, project, classpath);
            }
            let type_name = declared_type(symbol)?;
            resolve_type(&type_name, file, project, classpath)
        }
        ResolvedType::Classpath(name) => {
            let mut current = Some(name.as_str());
            let mut visited = HashSet::new();
            while let Some(class_name) = current {
                if !visited.insert(class_name.to_owned()) {
                    break;
                }
                let class = classpath.get_loose(class_name)?;
                for field in &class.fields {
                    if field.name == member {
                        if let Some(type_name) = field_type_name(&field.descriptor) {
                            if let Some(resolved) = resolve_fqn(&type_name, project, classpath) {
                                return Some(resolved);
                            }
                        }
                    }
                }
                for method in &class.methods {
                    if method.name == member {
                        if let Some(type_name) = method_return_type_name(&method.descriptor) {
                            if let Some(resolved) = resolve_fqn(&type_name, project, classpath) {
                                return Some(resolved);
                            }
                        }
                    }
                }
                current = class.super_name.as_deref();
            }
            None
        }
    }
}

fn declared_type(symbol: &JavaSymbol) -> Option<String> {
    let detail = symbol.detail.as_deref()?;
    let position = detail.rfind(&symbol.name)?;
    let before = detail[..position].trim_end();
    type_before_text(before)
}

fn members_for_type(
    resolved: &ResolvedType,
    project_index: Option<&ProjectIndex>,
    classpath: &ClasspathIndex,
) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    match resolved {
        ResolvedType::Project(location) => {
            if let Some(index) = project_index {
                if let Some(file) = index.files.get(&location.path) {
                    let container = type_container(file, location);
                    for symbol in &file.symbols {
                        if symbol.container.as_deref() != Some(container.as_str())
                            || symbol.kind == SymbolKind::Constructor
                        {
                            continue;
                        }
                        items.push(project_completion_item(symbol));
                    }
                }
            }
            items.extend(classpath_members("java.lang.Object", classpath));
        }
        ResolvedType::Classpath(name) => {
            items.extend(classpath_members(name, classpath));
        }
    }
    dedup_items(&mut items);
    items.truncate(800);
    items
}

fn project_completion_item(symbol: &JavaSymbol) -> CompletionItem {
    let kind = match symbol.kind {
        SymbolKind::Method => CompletionKind::Method,
        SymbolKind::Field | SymbolKind::EnumConstant => CompletionKind::Field,
        _ => CompletionKind::Type,
    };
    let insert_text = if kind == CompletionKind::Method {
        format!("{}()", symbol.name)
    } else {
        symbol.name.clone()
    };
    CompletionItem {
        label: symbol.name.clone(),
        detail: symbol
            .detail
            .clone()
            .unwrap_or_else(|| symbol.kind.label().to_owned()),
        kind,
        insert_text,
    }
}

fn classpath_members(name: &str, classpath: &ClasspathIndex) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    let mut current = Some(name.to_owned());
    let mut visited = HashSet::new();
    while let Some(class_name) = current {
        if !visited.insert(class_name.clone()) {
            break;
        }
        let Some(class) = classpath.get_loose(&class_name) else {
            break;
        };
        for field in &class.fields {
            items.push(CompletionItem {
                label: field.name.clone(),
                detail: format!(
                    "{}{}",
                    if field.is_static { "static " } else { "" },
                    field.signature
                ),
                kind: CompletionKind::Field,
                insert_text: field.name.clone(),
            });
        }
        for method in &class.methods {
            if method.name.starts_with('<') {
                continue;
            }
            items.push(CompletionItem {
                label: method.name.clone(),
                detail: format!(
                    "{}{}",
                    if method.is_static { "static " } else { "" },
                    method.signature
                ),
                kind: CompletionKind::Method,
                insert_text: format!("{}()", method.name),
            });
        }
        current = class.super_name.clone();
    }
    items
}

fn dedup_items(items: &mut Vec<CompletionItem>) {
    let mut seen = HashSet::new();
    items.retain(|item| seen.insert((item.label.clone(), item.detail.clone())));
}

fn project_type_items(project: &ProjectTypes, prefix: &str, items: &mut Vec<CompletionItem>) {
    let prefix_lower = prefix.to_lowercase();
    for fqn in project.by_fqn.keys() {
        let label = fqn.rsplit('.').next().unwrap_or(fqn);
        if !prefix.is_empty() && !label.to_lowercase().contains(&prefix_lower) {
            continue;
        }
        items.push(CompletionItem {
            label: label.to_owned(),
            detail: fqn.clone(),
            kind: CompletionKind::Type,
            insert_text: label.to_owned(),
        });
    }
}

fn type_items(
    file: &FileSymbols,
    prefix: &str,
    project: &ProjectTypes,
    classpath: &ClasspathIndex,
) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    project_type_items(project, prefix, &mut items);
    for fqn in completion_type_names(file, prefix, project, classpath) {
        let simple = fqn.rsplit('.').next().unwrap_or(&fqn);
        let label = simple.replace('$', ".");
        items.push(CompletionItem {
            label: label.clone(),
            detail: fqn,
            kind: CompletionKind::Type,
            insert_text: label,
        });
    }
    dedup_items(&mut items);
    items.truncate(800);
    items
}

fn package_type_items(
    receiver: &str,
    prefix: &str,
    project: &ProjectTypes,
    classpath: &ClasspathIndex,
) -> Vec<CompletionItem> {
    if !receiver
        .chars()
        .next()
        .is_some_and(|character| character.is_lowercase())
    {
        return Vec::new();
    }
    let package_prefix = format!("{receiver}.");
    let prefix_lower = prefix.to_lowercase();
    let mut items = Vec::new();
    for fqn in project.by_fqn.keys() {
        if !fqn.starts_with(&package_prefix) {
            continue;
        }
        let label = fqn[package_prefix.len()..].to_owned();
        if !prefix.is_empty() && !label.to_lowercase().starts_with(&prefix_lower) {
            continue;
        }
        items.push(CompletionItem {
            label: label.clone(),
            detail: fqn.clone(),
            kind: CompletionKind::Type,
            insert_text: label,
        });
    }
    for fqn in &classpath.type_names {
        if !fqn.starts_with(&package_prefix) {
            continue;
        }
        let label = &fqn[package_prefix.len()..];
        if label.contains('.') {
            continue;
        }
        if !prefix.is_empty() && !label.to_lowercase().starts_with(&prefix_lower) {
            continue;
        }
        items.push(CompletionItem {
            label: label.to_owned(),
            detail: fqn.clone(),
            kind: CompletionKind::Type,
            insert_text: label.to_owned(),
        });
        if items.len() >= 800 {
            break;
        }
    }
    items
}

fn type_container(file: &FileSymbols, location: &ProjectTypeLocation) -> String {
    let package = file.package.as_deref().unwrap_or("");
    if package.is_empty() {
        location.fqn.clone()
    } else {
        location
            .fqn
            .strip_prefix(&format!("{package}."))
            .unwrap_or(&location.fqn)
            .to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reclipse_classpath::{ClassInfo, MemberInfo, MemberKind, TypeKind};

    #[test]
    fn builds_project_type_fqn() {
        let file = FileSymbols {
            path: PathBuf::from("A.java"),
            package: Some("com.acme".to_owned()),
            imports: Vec::new(),
            import_lines: Vec::new(),
            symbols: Vec::new(),
            type_refs: Vec::new(),
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
            type_refs: Vec::new(),
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
    fn finds_semantic_type_references() {
        let user_path = PathBuf::from("User.java");
        let app_path = PathBuf::from("App.java");
        let user = crate::parse_source(&user_path, "package com.acme; public class User {}");
        let app = crate::parse_source(
            &app_path,
            "package com.acme; class App { User user; void f() { new User(); } }",
        );
        let project = reclipse_core::Project {
            root: PathBuf::from("."),
            name: "test".to_owned(),
            build_system: reclipse_core::BuildSystem::Plain,
            source_roots: Vec::new(),
            java_files: vec![user_path.clone(), app_path.clone()],
            all_files: vec![user_path.clone(), app_path.clone()],
        };
        let index = ProjectIndex::from_files(project, vec![user, app], 0);
        let types = ProjectTypes::build(&index);
        let references =
            find_type_references("com.acme.User", &types, &index, &ClasspathIndex::default());
        assert!(references
            .iter()
            .any(|reference| reference.file == user_path));
        assert!(
            references
                .iter()
                .filter(|reference| reference.file == app_path)
                .count()
                >= 2
        );
    }

    #[test]
    fn finds_semantic_type_references_through_import() {
        let user_path = PathBuf::from("com/acme/User.java");
        let app_path = PathBuf::from("com/other/App.java");
        let user = crate::parse_source(&user_path, "package com.acme; public class User {}");
        let app = crate::parse_source(
            &app_path,
            "package com.other; import com.acme.User; class App { User user; }",
        );
        let project = reclipse_core::Project {
            root: PathBuf::from("."),
            name: "test".to_owned(),
            build_system: reclipse_core::BuildSystem::Plain,
            source_roots: Vec::new(),
            java_files: vec![user_path, app_path.clone()],
            all_files: vec![app_path.clone()],
        };
        let index = ProjectIndex::from_files(project, vec![user, app], 0);
        let types = ProjectTypes::build(&index);
        let references =
            find_type_references("com.acme.User", &types, &index, &ClasspathIndex::default());
        assert!(references
            .iter()
            .any(|reference| reference.file == app_path));
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
            type_refs: Vec::new(),
            errors: Vec::new(),
            line_count: 1,
            parse_error: None,
        };
        let unresolved = unresolved_imports(&file, &project, &ClasspathIndex::default());
        assert_eq!(unresolved, vec![("java.util.List".to_owned(), 4)]);
    }

    #[test]
    fn infers_variable_types_across_lines_and_generics() {
        assert_eq!(
            infer_variable_type(
                "User user = new User();\nuser.",
                "User user = new User();\nuser.".chars().count(),
                "user"
            ),
            Some("User".to_owned())
        );
        assert_eq!(
            infer_variable_type(
                "Map<String, List<User>> users = new HashMap<>();\nusers.",
                "Map<String, List<User>> users = new HashMap<>();\nusers."
                    .chars()
                    .count(),
                "users"
            ),
            Some("Map".to_owned())
        );
        assert_eq!(
            infer_variable_type("String[] names;\nnames.", 20, "names"),
            Some("String".to_owned())
        );
    }

    fn string_classpath() -> ClasspathIndex {
        let method = |name: &str, descriptor: &str, signature: &str| MemberInfo {
            name: name.to_owned(),
            descriptor: descriptor.to_owned(),
            signature: signature.to_owned(),
            kind: MemberKind::Method,
            is_static: false,
            is_public: true,
        };
        let mut classpath = ClasspathIndex::default();
        classpath.classes.insert(
            "java.lang.String".to_owned(),
            ClassInfo {
                name: "java.lang.String".to_owned(),
                display_name: "String".to_owned(),
                package: "java.lang".to_owned(),
                kind: TypeKind::Class,
                is_public: true,
                super_name: Some("java.lang.Object".to_owned()),
                interfaces: Vec::new(),
                fields: Vec::new(),
                methods: vec![
                    method("length", "()I", "int length()"),
                    method(
                        "substring",
                        "(I)Ljava/lang/String;",
                        "String substring(int)",
                    ),
                ],
                origin: "test".to_owned(),
            },
        );
        classpath.classes.insert(
            "java.lang.Object".to_owned(),
            ClassInfo {
                name: "java.lang.Object".to_owned(),
                display_name: "Object".to_owned(),
                package: "java.lang".to_owned(),
                kind: TypeKind::Class,
                is_public: true,
                super_name: None,
                interfaces: Vec::new(),
                fields: Vec::new(),
                methods: vec![method(
                    "toString",
                    "()Ljava/lang/String;",
                    "String toString()",
                )],
                origin: "test".to_owned(),
            },
        );
        classpath
            .by_simple
            .insert("String".to_owned(), vec!["java.lang.String".to_owned()]);
        classpath.type_names = vec!["java.lang.Object".to_owned(), "java.lang.String".to_owned()];
        classpath
    }

    #[test]
    fn completes_classpath_members_after_receiver() {
        let classpath = string_classpath();
        let file = FileSymbols {
            path: PathBuf::from("App.java"),
            package: Some("com.acme".to_owned()),
            imports: Vec::new(),
            import_lines: Vec::new(),
            symbols: Vec::new(),
            type_refs: Vec::new(),
            errors: Vec::new(),
            line_count: 2,
            parse_error: None,
        };
        let source = "String value = \"x\";\nvalue.";
        let context = completion_context(
            source,
            source.chars().count(),
            &file,
            &ProjectTypes::default(),
            None,
            &classpath,
        );
        assert_eq!(context.receiver.as_deref(), Some("value"));
        assert_eq!(context.prefix, "");
        assert!(context
            .items
            .iter()
            .any(|item| item.label == "length" && item.kind == CompletionKind::Method));
        assert!(context.items.iter().any(|item| item.label == "toString"));
    }

    #[test]
    fn completes_project_members_after_receiver() {
        let user_path = PathBuf::from("User.java");
        let user_file = FileSymbols {
            path: user_path.clone(),
            package: Some("com.acme".to_owned()),
            imports: Vec::new(),
            import_lines: Vec::new(),
            symbols: vec![
                JavaSymbol {
                    name: "User".to_owned(),
                    kind: SymbolKind::Class,
                    line: 1,
                    column: 1,
                    container: None,
                    depth: 0,
                    detail: Some("public class User".to_owned()),
                },
                JavaSymbol {
                    name: "name".to_owned(),
                    kind: SymbolKind::Field,
                    line: 2,
                    column: 1,
                    container: Some("User".to_owned()),
                    depth: 1,
                    detail: Some("private final String name".to_owned()),
                },
                JavaSymbol {
                    name: "getName".to_owned(),
                    kind: SymbolKind::Method,
                    line: 3,
                    column: 1,
                    container: Some("User".to_owned()),
                    depth: 1,
                    detail: Some("public String getName()".to_owned()),
                },
            ],
            type_refs: Vec::new(),
            errors: Vec::new(),
            line_count: 3,
            parse_error: None,
        };
        let project = reclipse_core::Project {
            root: PathBuf::from("."),
            name: "test".to_owned(),
            build_system: reclipse_core::BuildSystem::Plain,
            source_roots: Vec::new(),
            java_files: vec![user_path.clone()],
            all_files: vec![user_path.clone()],
        };
        let index = ProjectIndex::from_files(project, vec![user_file], 0);
        let mut types = ProjectTypes::default();
        types.by_fqn.insert(
            "com.acme.User".to_owned(),
            ProjectTypeLocation {
                path: user_path,
                line: 1,
                column: 1,
                fqn: "com.acme.User".to_owned(),
                kind: SymbolKind::Class,
            },
        );
        let app_file = FileSymbols {
            path: PathBuf::from("App.java"),
            package: Some("com.acme".to_owned()),
            imports: Vec::new(),
            import_lines: Vec::new(),
            symbols: Vec::new(),
            type_refs: Vec::new(),
            errors: Vec::new(),
            line_count: 2,
            parse_error: None,
        };
        let source = "User user;\nuser.";
        let context = completion_context(
            source,
            source.chars().count(),
            &app_file,
            &types,
            Some(&index),
            &ClasspathIndex::default(),
        );
        assert!(context
            .items
            .iter()
            .any(|item| item.label == "name" && item.kind == CompletionKind::Field));
        assert!(context
            .items
            .iter()
            .any(|item| item.label == "getName" && item.kind == CompletionKind::Method));
    }
}
