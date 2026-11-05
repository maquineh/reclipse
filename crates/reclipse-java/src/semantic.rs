//! Lightweight semantic queries over the source index and classpath index.
//!
//! This is the M2 foundation: import/type resolution, unresolved import
//! diagnostics and type discovery. Full type inference and member completion
//! build on this layer.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use reclipse_classpath::{field_type_name, method_return_type_name, ClasspathIndex};

use crate::{FileSymbols, JavaSymbol, ProjectIndex, SymbolKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectTypeLocation {
    pub path: PathBuf,
    pub line: usize,
    pub column: usize,
    pub fqn: String,
    pub kind: SymbolKind,
}

#[derive(Debug, Clone)]
pub struct ProjectTypeRelations {
    pub file: PathBuf,
    pub super_types: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ProjectTypes {
    pub by_fqn: HashMap<String, ProjectTypeLocation>,
    pub by_simple: HashMap<String, Vec<String>>,
    pub packages: HashSet<String>,
    /// Raw superclass/interface names keyed by the type's FQN.
    pub relations: HashMap<String, ProjectTypeRelations>,
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
            for relation in &file.type_relations {
                let fqn = fqn_in_package(file.package.as_deref(), &relation.type_name);
                types.relations.insert(
                    fqn,
                    ProjectTypeRelations {
                        file: path.clone(),
                        super_types: relation.super_types.clone(),
                    },
                );
            }
            for symbol in &file.symbols {
                let implicit = match symbol.kind {
                    SymbolKind::Enum => Some("java.lang.Enum"),
                    SymbolKind::Record => Some("java.lang.Record"),
                    _ => None,
                };
                let Some(implicit) = implicit else {
                    continue;
                };
                let fqn = type_fqn(file, symbol);
                let relation = types
                    .relations
                    .entry(fqn)
                    .or_insert_with(|| ProjectTypeRelations {
                        file: path.clone(),
                        super_types: Vec::new(),
                    });
                if !relation.super_types.iter().any(|name| name == implicit) {
                    relation.super_types.push(implicit.to_owned());
                }
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

fn fqn_in_package(package: Option<&str>, name: &str) -> String {
    match package.filter(|package| !package.is_empty()) {
        Some(package) => format!("{package}.{name}"),
        None => name.to_owned(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

/// Identity of a method/field that can be searched or renamed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemberTarget {
    Project {
        /// File that declares the member.
        path: PathBuf,
        /// Type-chain container (`Outer.Inner`) of the declaration.
        container: String,
        /// FQN of the declaring type.
        type_fqn: String,
        name: String,
        is_call: bool,
    },
    Classpath {
        class: String,
        name: String,
        is_call: bool,
    },
}

impl MemberTarget {
    pub fn name(&self) -> &str {
        match self {
            Self::Project { name, .. } | Self::Classpath { name, .. } => name,
        }
    }

    pub fn is_call(&self) -> bool {
        match self {
            Self::Project { is_call, .. } | Self::Classpath { is_call, .. } => *is_call,
        }
    }

    pub fn owner_name(&self) -> &str {
        match self {
            Self::Project { type_fqn, .. } => type_fqn,
            Self::Classpath { class, .. } => class,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedMember {
    /// Type that actually declares the member (which may be a supertype).
    pub owner: ResolvedType,
    pub name: String,
    pub is_call: bool,
    /// Human-readable signature/detail.
    pub detail: String,
    /// Declared field/return type in raw source form.
    pub declared_type: Option<String>,
    /// File used to resolve `declared_type` for project members.
    pub declaring_file: Option<PathBuf>,
}

/// Resolve a member by name on a type, walking project and classpath supertypes.
pub fn resolve_member_of_type(
    ty: &ResolvedType,
    name: &str,
    is_call: Option<bool>,
    project: &ProjectTypes,
    project_index: Option<&ProjectIndex>,
    classpath: &ClasspathIndex,
) -> Option<ResolvedMember> {
    let mut visited = HashSet::new();
    resolve_member_of_type_inner(
        ty,
        name,
        is_call,
        project,
        project_index,
        classpath,
        &mut visited,
    )
}

fn resolve_member_of_type_inner(
    ty: &ResolvedType,
    name: &str,
    is_call: Option<bool>,
    project: &ProjectTypes,
    project_index: Option<&ProjectIndex>,
    classpath: &ClasspathIndex,
    visited: &mut HashSet<String>,
) -> Option<ResolvedMember> {
    if !visited.insert(ty.name().to_owned()) {
        return None;
    }
    match ty {
        ResolvedType::Project(location) => {
            let index = project_index?;
            let file = index.files.get(&location.path)?;
            let container = type_container(file, location);
            let symbol = file.symbols.iter().find(|symbol| {
                symbol.name == name
                    && symbol.container.as_deref() == Some(container.as_str())
                    && member_kind_matches(symbol.kind, is_call)
            });
            if let Some(symbol) = symbol {
                return Some(ResolvedMember {
                    owner: ty.clone(),
                    name: symbol.name.clone(),
                    is_call: symbol.kind == SymbolKind::Method,
                    detail: symbol
                        .detail
                        .clone()
                        .unwrap_or_else(|| symbol.kind.label().to_owned()),
                    declared_type: if symbol.kind == SymbolKind::EnumConstant {
                        Some(location.fqn.clone())
                    } else {
                        declared_type(symbol)
                    },
                    declaring_file: Some(location.path.clone()),
                });
            }
            for super_type in resolved_supertypes(&location.fqn, project, index, classpath) {
                if let Some(found) = resolve_member_of_type_inner(
                    &super_type,
                    name,
                    is_call,
                    project,
                    project_index,
                    classpath,
                    visited,
                ) {
                    return Some(found);
                }
            }
            None
        }
        ResolvedType::Classpath(class_name) => {
            let class = classpath.get_loose(class_name)?;
            let field = class
                .fields
                .iter()
                .find(|field| field.name == name && is_call.is_none_or(|is_call| !is_call));
            if let Some(field) = field {
                return Some(ResolvedMember {
                    owner: ty.clone(),
                    name: field.name.clone(),
                    is_call: false,
                    detail: format!(
                        "{}{}",
                        if field.is_static { "static " } else { "" },
                        field.signature
                    ),
                    declared_type: field_type_name(&field.descriptor),
                    declaring_file: None,
                });
            }
            let method = class.methods.iter().find(|method| {
                method.name == name
                    && !method.name.starts_with('<')
                    && is_call.is_none_or(|is_call| is_call)
            });
            if let Some(method) = method {
                return Some(ResolvedMember {
                    owner: ty.clone(),
                    name: method.name.clone(),
                    is_call: true,
                    detail: format!(
                        "{}{}",
                        if method.is_static { "static " } else { "" },
                        method.signature
                    ),
                    declared_type: method_return_type_name(&method.descriptor),
                    declaring_file: None,
                });
            }
            let mut supers = Vec::new();
            if let Some(super_name) = &class.super_name {
                supers.push(super_name.clone());
            }
            supers.extend(class.interfaces.iter().cloned());
            for super_name in supers {
                let Some(super_type) = resolve_fqn(&super_name, project, classpath) else {
                    continue;
                };
                if let Some(found) = resolve_member_of_type_inner(
                    &super_type,
                    name,
                    is_call,
                    project,
                    project_index,
                    classpath,
                    visited,
                ) {
                    return Some(found);
                }
            }
            None
        }
    }
}

fn member_kind_matches(kind: SymbolKind, is_call: Option<bool>) -> bool {
    match is_call {
        Some(true) => kind == SymbolKind::Method,
        Some(false) => matches!(kind, SymbolKind::Field | SymbolKind::EnumConstant),
        None => matches!(
            kind,
            SymbolKind::Method | SymbolKind::Field | SymbolKind::EnumConstant
        ),
    }
}

fn resolved_supertypes(
    fqn: &str,
    project: &ProjectTypes,
    index: &ProjectIndex,
    classpath: &ClasspathIndex,
) -> Vec<ResolvedType> {
    let Some(relations) = project.relations.get(fqn) else {
        return Vec::new();
    };
    let Some(file) = index.files.get(&relations.file) else {
        return Vec::new();
    };
    relations
        .super_types
        .iter()
        .filter_map(|name| resolve_type(name, file, project, classpath))
        .collect()
}

/// Direct supertypes of a project type resolved through its declaring file.
pub fn supertypes_of(
    fqn: &str,
    project: &ProjectTypes,
    index: &ProjectIndex,
    classpath: &ClasspathIndex,
) -> Vec<ResolvedType> {
    resolved_supertypes(fqn, project, index, classpath)
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

#[derive(Debug, Clone)]
pub struct MemberReference {
    pub file: PathBuf,
    pub name: String,
    pub line: usize,
    pub column: usize,
    pub start_byte: usize,
    pub end_byte: usize,
    pub is_call: bool,
}

/// Find semantic references to a method/field, including inherited receivers
/// and method references (`Type::method`). Declaration names are not included;
/// callers add them from the project index.
pub fn find_member_references(
    target: &MemberTarget,
    project: &ProjectTypes,
    index: &ProjectIndex,
    classpath: &ClasspathIndex,
) -> Vec<MemberReference> {
    let mut references = Vec::new();
    for (path, file) in &index.files {
        let candidates: Vec<_> = file
            .member_refs
            .iter()
            .filter(|reference| {
                reference.name == target.name() && reference.is_call == target.is_call()
            })
            .collect();
        if candidates.is_empty() {
            continue;
        }
        let Ok(source) = fs::read_to_string(path) else {
            continue;
        };
        let bindings = if candidates
            .iter()
            .any(|reference| reference.qualifier.is_none())
        {
            crate::parse_local_bindings(&source)
        } else {
            Vec::new()
        };
        for reference in candidates {
            if !reference.is_call
                && reference.qualifier.is_none()
                && local_binding_covers(&bindings, &reference.name, reference.start_byte)
            {
                continue;
            }
            let Some(resolved) = resolve_reference_member(
                reference,
                &source,
                file,
                project,
                Some(index),
                classpath,
                &bindings,
            ) else {
                continue;
            };
            if member_owner_matches(target, &resolved.owner) {
                references.push(MemberReference {
                    file: path.clone(),
                    name: reference.name.clone(),
                    line: reference.line,
                    column: reference.column,
                    start_byte: reference.start_byte,
                    end_byte: reference.end_byte,
                    is_call: reference.is_call,
                });
            }
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

/// Resolve the member under `cursor` (character index) to a rename/reference
/// target. Falls back to a unique project declaration or a same-file
/// declaration when the occurrence cannot be resolved.
pub fn member_target_at(
    source: &str,
    file: &FileSymbols,
    cursor: usize,
    project: &ProjectTypes,
    index: &ProjectIndex,
    classpath: &ClasspathIndex,
) -> Option<MemberTarget> {
    let (word, start, _) = identifier_at_char(source, cursor)?;
    let start_byte = byte_index_of_char(source, start);
    let bindings = crate::parse_local_bindings(source);
    if let Some(reference) = file
        .member_refs
        .iter()
        .find(|reference| reference.start_byte == start_byte)
    {
        if let Some(resolved) = resolve_reference_member(
            reference,
            source,
            file,
            project,
            Some(index),
            classpath,
            &bindings,
        ) {
            return member_target_from_resolved(&resolved, index);
        }
    }

    // Cursor on a declaration (or a stale occurrence): use the syntax index.
    let declaration = file.symbols.iter().find(|symbol| {
        symbol.name == word
            && !symbol.kind.is_type()
            && symbol.kind != SymbolKind::Constructor
            && symbol.name_start_byte <= start_byte
            && start_byte < symbol.name_end_byte
    });
    if let Some(symbol) = declaration {
        return project_member_target(file, symbol);
    }
    if let Some(symbol) = file.symbols.iter().find(|symbol| {
        symbol.name == word && !symbol.kind.is_type() && symbol.kind != SymbolKind::Constructor
    }) {
        return project_member_target(file, symbol);
    }
    None
}

fn member_target_from_resolved(
    resolved: &ResolvedMember,
    index: &ProjectIndex,
) -> Option<MemberTarget> {
    match &resolved.owner {
        ResolvedType::Project(location) => {
            let file = index.files.get(&location.path)?;
            let container = type_container(file, location);
            Some(MemberTarget::Project {
                path: location.path.clone(),
                container,
                type_fqn: location.fqn.clone(),
                name: resolved.name.clone(),
                is_call: resolved.is_call,
            })
        }
        ResolvedType::Classpath(class) => Some(MemberTarget::Classpath {
            class: class.clone(),
            name: resolved.name.clone(),
            is_call: resolved.is_call,
        }),
    }
}

fn project_member_target(file: &FileSymbols, symbol: &JavaSymbol) -> Option<MemberTarget> {
    let container = symbol.container.clone()?;
    let type_fqn = fqn_in_package(file.package.as_deref(), &container);
    Some(MemberTarget::Project {
        path: file.path.clone(),
        container,
        type_fqn,
        name: symbol.name.clone(),
        is_call: symbol.kind == SymbolKind::Method,
    })
}

fn member_owner_matches(target: &MemberTarget, owner: &ResolvedType) -> bool {
    match (target, owner) {
        (MemberTarget::Project { type_fqn, .. }, ResolvedType::Project(location)) => {
            type_fqn == &location.fqn
        }
        (MemberTarget::Classpath { class, .. }, ResolvedType::Classpath(name)) => class == name,
        _ => false,
    }
}

fn resolve_reference_member(
    reference: &crate::JavaMemberReference,
    source: &str,
    file: &FileSymbols,
    project: &ProjectTypes,
    index: Option<&ProjectIndex>,
    classpath: &ClasspathIndex,
    bindings: &[crate::LocalBinding],
) -> Option<ResolvedMember> {
    let ty = match &reference.qualifier {
        Some(qualifier) => resolve_expression_type(
            qualifier,
            source,
            char_index_of_byte(source, reference.start_byte),
            file,
            project,
            index,
            classpath,
        )?,
        None => {
            if !reference.is_call
                && local_binding_covers(bindings, &reference.name, reference.start_byte)
            {
                return None;
            }
            if let Some(container) = reference.container.as_deref() {
                let fqn = fqn_in_package(file.package.as_deref(), container);
                if let Some(enclosing) = resolve_fqn(&fqn, project, classpath) {
                    if let Some(found) = resolve_member_of_type(
                        &enclosing,
                        &reference.name,
                        Some(reference.is_call),
                        project,
                        index,
                        classpath,
                    ) {
                        return Some(found);
                    }
                }
            }
            return resolve_static_import_member(
                file,
                &reference.name,
                reference.is_call,
                project,
                index,
                classpath,
            );
        }
    };
    resolve_member_of_type(
        &ty,
        &reference.name,
        Some(reference.is_call),
        project,
        index,
        classpath,
    )
}

fn resolve_static_import_member(
    file: &FileSymbols,
    name: &str,
    is_call: bool,
    project: &ProjectTypes,
    index: Option<&ProjectIndex>,
    classpath: &ClasspathIndex,
) -> Option<ResolvedMember> {
    for import in &file.imports {
        let Some(path) = import.trim().strip_prefix("static ") else {
            continue;
        };
        let Some((owner, member)) = path.rsplit_once('.') else {
            continue;
        };
        if member != "*" && member != name {
            continue;
        }
        let Some(owner_type) = resolve_type(owner, file, project, classpath) else {
            continue;
        };
        if let Some(found) =
            resolve_member_of_type(&owner_type, name, Some(is_call), project, index, classpath)
        {
            return Some(found);
        }
    }
    None
}

fn local_binding_covers(bindings: &[crate::LocalBinding], name: &str, byte: usize) -> bool {
    bindings.iter().any(|binding| {
        binding.name == name
            && binding.scope_start_byte <= byte
            && byte < binding.scope_end_byte
            && binding.declaration_start_byte <= byte
    })
}

fn identifier_at_char(source: &str, cursor: usize) -> Option<(String, usize, usize)> {
    let characters: Vec<char> = source.chars().collect();
    if characters.is_empty() {
        return None;
    }
    let mut index = cursor.min(characters.len() - 1);
    if !is_identifier_char(characters[index]) {
        if index > 0 && is_identifier_char(characters[index - 1]) {
            index -= 1;
        } else if index + 1 < characters.len() && is_identifier_char(characters[index + 1]) {
            index += 1;
        } else {
            return None;
        }
    }
    let mut start = index;
    while start > 0 && is_identifier_char(characters[start - 1]) {
        start -= 1;
    }
    let mut end = index + 1;
    while end < characters.len() && is_identifier_char(characters[end]) {
        end += 1;
    }
    Some((characters[start..end].iter().collect(), start, end))
}

fn char_index_of_byte(source: &str, byte: usize) -> usize {
    source
        .get(..byte.min(source.len()))
        .map(|prefix| prefix.chars().count())
        .unwrap_or(0)
}

fn byte_index_of_char(source: &str, char_index: usize) -> usize {
    source
        .char_indices()
        .nth(char_index)
        .map(|(byte, _)| byte)
        .unwrap_or(source.len())
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
        if ends_with_keyword(before, "var") {
            if let Some(type_name) = var_initializer_type(&prefix[end..]) {
                return Some(type_name);
            }
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

fn ends_with_keyword(text: &str, keyword: &str) -> bool {
    let text = text.trim_end();
    text.ends_with(keyword)
        && text
            .char_indices()
            .nth_back(keyword.chars().count())
            .is_none_or(|(_, character)| !is_identifier_char(character))
}

/// Infer `var` types from simple initializers: `new Type(...)`, string,
/// boolean and numeric literals. More complex expressions stay unresolved.
fn var_initializer_type(rest: &str) -> Option<String> {
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    if rest.starts_with('"') {
        return Some("String".to_owned());
    }
    if rest.starts_with('\'') {
        return Some("char".to_owned());
    }
    if rest.starts_with("true") || rest.starts_with("false") {
        return Some("boolean".to_owned());
    }
    if let Some(after_new) = rest.strip_prefix("new") {
        let after_new = after_new.trim_start();
        let end = after_new
            .char_indices()
            .take_while(|(_, character)| is_identifier_char(*character) || *character == '.')
            .last()
            .map(|(index, character)| index + character.len_utf8())
            .unwrap_or(0);
        let token = after_new[..end].trim_start_matches('.');
        let simple = token.rsplit('.').next().unwrap_or(token);
        if simple
            .chars()
            .next()
            .is_some_and(|character| character.is_alphabetic())
        {
            return Some(simple.to_owned());
        }
    }
    if rest
        .chars()
        .next()
        .is_some_and(|character| character.is_ascii_digit())
    {
        return Some(
            if rest.contains('.') || rest.contains('f') || rest.contains('d') {
                "double"
            } else {
                "int"
            }
            .to_owned(),
        );
    }
    None
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
            Some(resolved) => members_for_type(&resolved, project, project_index, classpath),
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
    let mut begin = index;
    loop {
        while begin > 0 && characters[begin - 1].is_whitespace() {
            begin -= 1;
        }
        if begin == 0 {
            break;
        }
        match characters[begin - 1] {
            ')' | ']' => {
                let open = matching_open_delimiter(characters, begin - 1)?;
                begin = open;
                let mut name_end = begin;
                while name_end > 0 && is_identifier_char(characters[name_end - 1]) {
                    name_end -= 1;
                }
                if name_end < begin {
                    begin = name_end;
                }
            }
            character if is_identifier_char(character) => {
                while begin > 0 && is_identifier_char(characters[begin - 1]) {
                    begin -= 1;
                }
            }
            '.' => {
                begin -= 1;
                continue;
            }
            _ => break,
        }
        let mut probe = begin;
        while probe > 0 && characters[probe - 1].is_whitespace() {
            probe -= 1;
        }
        if probe > 0 && characters[probe - 1] != '.' {
            break;
        }
    }
    let receiver: String = characters[begin..end].iter().collect();
    let receiver = receiver.trim_matches('.').trim().to_owned();
    if receiver.is_empty() {
        None
    } else {
        Some(receiver)
    }
}

fn matching_open_delimiter(characters: &[char], close_index: usize) -> Option<usize> {
    let (open, close) = match characters.get(close_index)? {
        ')' => ('(', ')'),
        ']' => ('[', ']'),
        _ => return None,
    };
    let mut depth = 0usize;
    for index in (0..=close_index).rev() {
        if characters[index] == close {
            depth += 1;
        } else if characters[index] == open {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
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
    resolve_expression_type(
        receiver,
        source,
        cursor,
        file,
        project,
        project_index,
        classpath,
    )
}

/// Resolve an expression chain such as `app.service`, `this.cache` or
/// `String.valueOf`. Used by completion, references and rename.
pub fn resolve_expression_type(
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
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    let first = *segments.first()?;
    let mut current = if first == "this" {
        let fqn = active_type_fqn_at(file, source, cursor)?;
        resolve_fqn(&fqn, project, classpath)?
    } else if first == "super" {
        let fqn = active_type_fqn_at(file, source, cursor)?;
        let owner = resolve_fqn(&fqn, project, classpath)?;
        super_type_of(&owner, project, project_index, classpath)?
    } else if let Some(name) = method_call_name(first) {
        let fqn = active_type_fqn_at(file, source, cursor)?;
        let owner = resolve_fqn(&fqn, project, classpath)?;
        member_type(&owner, name, project, project_index, classpath)?
    } else {
        match resolve_type(first, file, project, classpath) {
            Some(resolved) => resolved,
            None => {
                let inferred = infer_variable_type(source, cursor, member_segment_name(first))?;
                resolve_type(&inferred, file, project, classpath)?
            }
        }
    };
    for segment in &segments[1..] {
        let name = member_segment_name(segment);
        current = member_type(&current, name, project, project_index, classpath)?;
    }
    Some(current)
}

/// Extract the member name from `find()`, `find(1)` or `users[0]`.
fn member_segment_name(segment: &str) -> &str {
    let segment = segment.trim();
    let name = segment
        .split_once('(')
        .map(|(name, _)| name)
        .or_else(|| segment.split_once('[').map(|(name, _)| name))
        .unwrap_or(segment)
        .trim();
    if name.is_empty() {
        segment
    } else {
        name
    }
}

fn method_call_name(segment: &str) -> Option<&str> {
    let (name, _) = segment.trim().split_once('(')?;
    let name = name.trim();
    (!name.is_empty()).then_some(name)
}

fn active_type_fqn_at(file: &FileSymbols, source: &str, cursor: usize) -> Option<String> {
    let line = source
        .chars()
        .take(cursor)
        .filter(|character| *character == '\n')
        .count()
        + 1;
    file.symbols
        .iter()
        .filter(|symbol| symbol.kind.is_type() && symbol.line <= line)
        .max_by_key(|symbol| (symbol.line, symbol.depth))
        .map(|symbol| type_fqn(file, symbol))
}

fn super_type_of(
    ty: &ResolvedType,
    project: &ProjectTypes,
    project_index: Option<&ProjectIndex>,
    classpath: &ClasspathIndex,
) -> Option<ResolvedType> {
    match ty {
        ResolvedType::Project(location) => {
            let index = project_index?;
            resolved_supertypes(&location.fqn, project, index, classpath)
                .into_iter()
                .next()
        }
        ResolvedType::Classpath(name) => {
            let class = classpath.get_loose(name)?;
            class
                .super_name
                .as_deref()
                .and_then(|super_name| resolve_fqn(super_name, project, classpath))
                .or_else(|| {
                    class
                        .interfaces
                        .iter()
                        .find_map(|name| resolve_fqn(name, project, classpath))
                })
        }
    }
}

fn member_type(
    receiver: &ResolvedType,
    member: &str,
    project: &ProjectTypes,
    project_index: Option<&ProjectIndex>,
    classpath: &ClasspathIndex,
) -> Option<ResolvedType> {
    let resolved =
        resolve_member_of_type(receiver, member, None, project, project_index, classpath)?;
    let declared = resolved.declared_type?;
    match &resolved.owner {
        ResolvedType::Project(location) => {
            let index = project_index?;
            let file = index.files.get(&location.path)?;
            resolve_type(&declared, file, project, classpath)
        }
        ResolvedType::Classpath(_) => resolve_fqn(&declared, project, classpath),
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
    project: &ProjectTypes,
    project_index: Option<&ProjectIndex>,
    classpath: &ClasspathIndex,
) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    let mut visited = HashSet::new();
    collect_member_items(
        resolved,
        project,
        project_index,
        classpath,
        &mut visited,
        &mut items,
    );
    dedup_items(&mut items);
    items.truncate(800);
    items
}

fn collect_member_items(
    ty: &ResolvedType,
    project: &ProjectTypes,
    project_index: Option<&ProjectIndex>,
    classpath: &ClasspathIndex,
    visited: &mut HashSet<String>,
    items: &mut Vec<CompletionItem>,
) {
    if !visited.insert(ty.name().to_owned()) {
        return;
    }
    match ty {
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
                    for super_type in resolved_supertypes(&location.fqn, project, index, classpath)
                    {
                        collect_member_items(
                            &super_type,
                            project,
                            project_index,
                            classpath,
                            visited,
                            items,
                        );
                    }
                    // Every class inherits Object even without an explicit
                    // `extends`.
                    if !visited.contains("java.lang.Object") {
                        collect_member_items(
                            &ResolvedType::Classpath("java.lang.Object".to_owned()),
                            project,
                            project_index,
                            classpath,
                            visited,
                            items,
                        );
                    }
                }
            }
        }
        ResolvedType::Classpath(name) => {
            let Some(class) = classpath.get_loose(name) else {
                return;
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
            let mut supers = Vec::new();
            if let Some(super_name) = &class.super_name {
                supers.push(super_name.clone());
            }
            supers.extend(class.interfaces.iter().cloned());
            for super_name in supers {
                if let Some(super_type) = resolve_fqn(&super_name, project, classpath) {
                    collect_member_items(
                        &super_type,
                        project,
                        project_index,
                        classpath,
                        visited,
                        items,
                    );
                }
            }
        }
    }
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

    fn empty_file(path: &str, package: Option<&str>) -> FileSymbols {
        let mut file = FileSymbols::empty(PathBuf::from(path).as_path());
        file.package = package.map(str::to_owned);
        file.line_count = 1;
        file
    }

    fn symbol(name: &str, kind: SymbolKind, container: Option<&str>, detail: &str) -> JavaSymbol {
        JavaSymbol {
            name: name.to_owned(),
            kind,
            line: 1,
            column: 1,
            container: container.map(str::to_owned),
            depth: container.is_some() as usize,
            detail: Some(detail.to_owned()),
            name_start_byte: 0,
            name_end_byte: name.len(),
        }
    }

    #[test]
    fn builds_project_type_fqn() {
        let file = empty_file("A.java", Some("com.acme"));
        let symbol = symbol("Inner", SymbolKind::Class, Some("Outer"), "class Inner");
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
        let mut file = empty_file("App.java", Some("com.acme"));
        file.imports = vec!["com.acme.User".to_owned()];
        file.import_lines = vec![3];
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
        let mut file = empty_file("App.java", Some("com.acme"));
        file.imports = vec!["java.util.List".to_owned()];
        file.import_lines = vec![4];
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
        assert_eq!(
            infer_variable_type(
                "var user = new com.acme.User();\nuser.",
                "var user = new com.acme.User();\nuser.".chars().count(),
                "user"
            ),
            Some("User".to_owned())
        );
        assert_eq!(
            infer_variable_type(
                "var text = \"hello\";\ntext.",
                "var text = \"hello\";\ntext.".chars().count(),
                "text"
            ),
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
        let file = empty_file("App.java", Some("com.acme"));
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

    fn project_with_files(files: Vec<(PathBuf, &str)>) -> (ProjectIndex, ProjectTypes) {
        let parsed: Vec<_> = files
            .iter()
            .map(|(path, source)| crate::parse_source(path, source))
            .collect();
        let project = reclipse_core::Project {
            root: PathBuf::from("."),
            name: "test".to_owned(),
            build_system: reclipse_core::BuildSystem::Plain,
            source_roots: Vec::new(),
            java_files: files.iter().map(|(path, _)| path.clone()).collect(),
            all_files: files.iter().map(|(path, _)| path.clone()).collect(),
        };
        let index = ProjectIndex::from_files(project, parsed, 0);
        let types = ProjectTypes::build(&index);
        (index, types)
    }

    #[test]
    fn completes_project_members_after_receiver() {
        let (index, types) = project_with_files(vec![(
            PathBuf::from("com/acme/User.java"),
            "package com.acme; public class User { private final String name; public String getName() { return name; } }",
        )]);
        let app = empty_file("com/acme/App.java", Some("com.acme"));
        let source = "User user;\nuser.";
        let context = completion_context(
            source,
            source.chars().count(),
            &app,
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

    #[test]
    fn completes_inherited_project_members() {
        let (index, types) = project_with_files(vec![
            (
                PathBuf::from("com/acme/Base.java"),
                "package com.acme; public class Base { protected String baseName; public void baseRun() {} }",
            ),
            (
                PathBuf::from("com/acme/Sub.java"),
                "package com.acme; public class Sub extends Base { private int extra; }",
            ),
        ]);
        let app = empty_file("com/acme/App.java", Some("com.acme"));
        let source = "Sub sub = new Sub();\nsub.";
        let context = completion_context(
            source,
            source.chars().count(),
            &app,
            &types,
            Some(&index),
            &ClasspathIndex::default(),
        );
        assert!(context.items.iter().any(|item| item.label == "baseName"));
        assert!(context.items.iter().any(|item| item.label == "baseRun"));
        assert!(context.items.iter().any(|item| item.label == "extra"));
    }

    #[test]
    fn completes_chained_method_return() {
        let (index, types) = project_with_files(vec![
            (
                PathBuf::from("com/acme/User.java"),
                "package com.acme; public class User { public String displayName() { return \"\"; } }",
            ),
            (
                PathBuf::from("com/acme/UserService.java"),
                "package com.acme; public class UserService { public User find() { return null; } }",
            ),
        ]);
        let app = empty_file("com/acme/App.java", Some("com.acme"));
        let source = "UserService service = new UserService();\nservice.find().";
        let context = completion_context(
            source,
            source.chars().count(),
            &app,
            &types,
            Some(&index),
            &ClasspathIndex::default(),
        );
        assert_eq!(context.receiver.as_deref(), Some("service.find()"));
        assert!(context.items.iter().any(|item| item.label == "displayName"));
    }

    fn write_member_project() -> (
        tempfile::TempDir,
        PathBuf,
        PathBuf,
        ProjectIndex,
        ProjectTypes,
        FileSymbols,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let user_path = dir.path().join("com/acme/User.java");
        let app_path = dir.path().join("com/acme/App.java");
        std::fs::create_dir_all(user_path.parent().unwrap()).unwrap();
        let user_source = "package com.acme; public class User { private String name; public void save() {} public void save(int times) {} }";
        let app_source = "package com.acme; class App { User user; String name; void run() { user.save(); user.save(1); this.save(); String name = \"x\"; name.length(); } }";
        std::fs::write(&user_path, user_source).unwrap();
        std::fs::write(&app_path, app_source).unwrap();
        let user = crate::parse_source(&user_path, user_source);
        let app = crate::parse_source(&app_path, app_source);
        let project = reclipse_core::Project {
            root: dir.path().to_path_buf(),
            name: "test".to_owned(),
            build_system: reclipse_core::BuildSystem::Plain,
            source_roots: Vec::new(),
            java_files: vec![user_path.clone(), app_path.clone()],
            all_files: vec![user_path.clone(), app_path.clone()],
        };
        let index = ProjectIndex::from_files(project, vec![user, app.clone()], 0);
        let types = ProjectTypes::build(&index);
        (dir, user_path, app_path, index, types, app)
    }

    #[test]
    fn finds_semantic_member_references() {
        let (_dir, _user_path, _app_path, index, types, app) = write_member_project();
        let target = MemberTarget::Project {
            path: PathBuf::from("com/acme/User.java"),
            container: "User".to_owned(),
            type_fqn: "com.acme.User".to_owned(),
            name: "save".to_owned(),
            is_call: true,
        };
        let references =
            find_member_references(&target, &types, &index, &ClasspathIndex::default());
        assert_eq!(references.len(), 2);
        assert!(references
            .iter()
            .all(|reference| { reference.file.ends_with("App.java") }));
        let source = std::fs::read_to_string(&_app_path).unwrap();
        let cursor = source.find("user.save(1)").unwrap() + "user.".len();
        let found = member_target_at(
            &source,
            &app,
            source[..cursor].chars().count(),
            &types,
            &index,
            &ClasspathIndex::default(),
        )
        .unwrap();
        assert_eq!(found.name(), "save");
        assert_eq!(found.owner_name(), "com.acme.User");
    }

    #[test]
    fn member_references_ignore_local_shadowing() {
        let (dir, user_path, app_path, _index, _types, _app) = write_member_project();
        let user_source = "package com.acme; public class User { public String name; }";
        let app_source = "package com.acme; class App { String name; void run() { String name = \"x\"; name.length(); } }";
        std::fs::write(&user_path, user_source).unwrap();
        std::fs::write(&app_path, app_source).unwrap();
        let user = crate::parse_source(&user_path, user_source);
        let app = crate::parse_source(&app_path, app_source);
        let project = reclipse_core::Project {
            root: dir.path().to_path_buf(),
            name: "test".to_owned(),
            build_system: reclipse_core::BuildSystem::Plain,
            source_roots: Vec::new(),
            java_files: vec![user_path, app_path],
            all_files: Vec::new(),
        };
        let index = ProjectIndex::from_files(project, vec![user, app], 0);
        let types = ProjectTypes::build(&index);
        let target = MemberTarget::Project {
            path: PathBuf::from("com/acme/User.java"),
            container: "User".to_owned(),
            type_fqn: "com.acme.User".to_owned(),
            name: "name".to_owned(),
            is_call: false,
        };
        assert!(
            find_member_references(&target, &types, &index, &ClasspathIndex::default()).is_empty()
        );
    }
}
