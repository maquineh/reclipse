//! Lightweight semantic queries over the source index and classpath index.
//!
//! This is the M2 foundation: import/type resolution, unresolved import
//! diagnostics and type discovery. Full type inference and member completion
//! build on this layer.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use reclipse_classpath::{
    field_type_name, method_return_type_name, signature_params_text, signature_return_text,
    signature_type_params, ClasspathIndex,
};

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
    /// Generic parameter texts of the declaration, e.g. [`T`] for
    /// `<T> T identity(T x)`. Used to bind type variables from call args.
    pub param_generics: Vec<String>,
    /// JVM generic `Signature` of the member when it comes from the classpath.
    pub generic_sig: Option<String>,
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
    resolve_member_of_type_with_arity(ty, name, is_call, None, project, project_index, classpath)
}

/// Like [`resolve_member_of_type`], preferring an overload with `argument_count`
/// arguments when several methods share the same name.
pub fn resolve_member_of_type_with_arity(
    ty: &ResolvedType,
    name: &str,
    is_call: Option<bool>,
    argument_count: Option<usize>,
    project: &ProjectTypes,
    project_index: Option<&ProjectIndex>,
    classpath: &ClasspathIndex,
) -> Option<ResolvedMember> {
    resolve_member_of_type_with_args(
        ty,
        name,
        is_call,
        argument_count,
        None,
        project,
        project_index,
        classpath,
    )
}

/// Overload-aware resolution with inferred argument types when available.
/// `argument_types` uses simple names (`String`, `int`, `null`); `None` entries
/// are neutral. Arity mismatches are always rejected first.
#[allow(clippy::too_many_arguments)]
pub fn resolve_member_of_type_with_args(
    ty: &ResolvedType,
    name: &str,
    is_call: Option<bool>,
    argument_count: Option<usize>,
    argument_types: Option<&[Option<String>]>,
    project: &ProjectTypes,
    project_index: Option<&ProjectIndex>,
    classpath: &ClasspathIndex,
) -> Option<ResolvedMember> {
    let mut visited = HashSet::new();
    resolve_member_of_type_inner(
        ty,
        name,
        is_call,
        argument_count,
        argument_types,
        project,
        project_index,
        classpath,
        &mut visited,
    )
}

#[allow(clippy::too_many_arguments)]
fn resolve_member_of_type_inner(
    ty: &ResolvedType,
    name: &str,
    is_call: Option<bool>,
    argument_count: Option<usize>,
    argument_types: Option<&[Option<String>]>,
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
            let mut candidates = file.symbols.iter().filter(|symbol| {
                symbol.name == name
                    && symbol.container.as_deref() == Some(container.as_str())
                    && member_kind_matches(symbol.kind, is_call)
            });
            let first = candidates.next();
            let subtype = |sub: &str, sup: &str| {
                is_subtype_simple(sub, sup, project, project_index, classpath)
            };
            let symbol = if is_call == Some(true) {
                first.and_then(|first| {
                    let all: Vec<_> = std::iter::once(first).chain(candidates).collect();
                    if let Some(arg_types) = argument_types {
                        let sigs: Vec<(Vec<String>, bool)> = all
                            .iter()
                            .map(|symbol| {
                                symbol
                                    .detail
                                    .as_deref()
                                    .and_then(detail_param_list)
                                    .unwrap_or_default()
                            })
                            .collect();
                        if let Some(index) = pick_overload(&sigs, arg_types, &subtype) {
                            return Some(all[index]);
                        }
                    }
                    let Some(argument_count) = argument_count else {
                        return Some(first);
                    };
                    all.into_iter()
                        .find(|symbol| {
                            symbol.detail.as_deref().is_some_and(|detail| {
                                parameter_count(detail) == Some(argument_count)
                            })
                        })
                        .or(Some(first))
                })
            } else {
                first
            };
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
                    param_generics: symbol
                        .detail
                        .as_deref()
                        .map(detail_param_generics)
                        .unwrap_or_default(),
                    generic_sig: None,
                    declaring_file: Some(location.path.clone()),
                });
            }
            for super_type in resolved_supertypes(&location.fqn, project, index, classpath) {
                if let Some(found) = resolve_member_of_type_inner(
                    &super_type,
                    name,
                    is_call,
                    argument_count,
                    argument_types,
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
                    param_generics: Vec::new(),
                    generic_sig: field.generic_signature.clone(),
                    declaring_file: None,
                });
            }
            let mut methods = class.methods.iter().filter(|method| {
                method.name == name
                    && !method.name.starts_with('<')
                    && is_call.is_none_or(|is_call| is_call)
            });
            let first_method = methods.next();
            let subtype = |sub: &str, sup: &str| {
                is_subtype_simple(sub, sup, project, project_index, classpath)
            };
            let method = if is_call == Some(true) {
                first_method.and_then(|first| {
                    let all: Vec<_> = std::iter::once(first).chain(methods).collect();
                    if let Some(arg_types) = argument_types {
                        let sigs: Vec<(Vec<String>, bool)> = all
                            .iter()
                            .map(|method| {
                                (
                                    method_param_types(&method.descriptor).unwrap_or_default(),
                                    method.is_varargs,
                                )
                            })
                            .collect();
                        if let Some(index) = pick_overload(&sigs, arg_types, &subtype) {
                            return Some(all[index]);
                        }
                    }
                    let Some(argument_count) = argument_count else {
                        return Some(first);
                    };
                    all.into_iter()
                        .find(|method| {
                            method_argument_count(&method.descriptor) == Some(argument_count)
                        })
                        .or(Some(first))
                })
            } else {
                first_method
            };
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
                    param_generics: method
                        .generic_signature
                        .as_deref()
                        .and_then(signature_params_text)
                        .unwrap_or_default(),
                    generic_sig: method.generic_signature.clone(),
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
                    argument_count,
                    argument_types,
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
    let simple = fqn.rsplit('.').next().unwrap_or(fqn);
    for (path, file) in &index.files {
        // Local classes are not project types; skip files that shadow the
        // target's simple name to avoid renaming a local declaration.
        if file.local_type_names.iter().any(|name| name == simple) {
            continue;
        }
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
        Some(qualifier) => {
            if reference
                .container
                .as_deref()
                .is_some_and(|container| container.contains("<anonymous>"))
                && matches!(qualifier.as_str(), "this" | "super")
            {
                return None;
            }
            resolve_expression_type(
                qualifier,
                source,
                char_index_of_byte(source, reference.start_byte),
                file,
                project,
                index,
                classpath,
            )?
        }
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
    resolve_member_of_type_with_args(
        &ty,
        &reference.name,
        Some(reference.is_call),
        reference.argument_count,
        reference_argument_types(reference, source, file, project, index, classpath).as_deref(),
        project,
        index,
        classpath,
    )
}

/// Extract call argument texts after a member name and infer their types.
/// Returns `None` when the reference is not a call.
fn reference_argument_types(
    reference: &crate::JavaMemberReference,
    source: &str,
    file: &FileSymbols,
    project: &ProjectTypes,
    index: Option<&ProjectIndex>,
    classpath: &ClasspathIndex,
) -> Option<Vec<Option<String>>> {
    if !reference.is_call {
        return None;
    }
    let texts = argument_texts_at(source, reference.start_byte)?;
    let cursor = char_index_of_byte(source, reference.start_byte);
    Some(
        texts
            .into_iter()
            .map(|arg| {
                infer_arg_simple(arg.trim(), source, cursor, file, project, index, classpath)
            })
            .collect(),
    )
}

/// Find `(...)` after a member name byte offset and split its arguments.
fn argument_texts_at(source: &str, name_start: usize) -> Option<Vec<String>> {
    let rest = source.get(name_start..)?;
    let open = rest.find('(')?;
    let mut depth = 0i32;
    let mut close = None;
    for (i, c) in rest.char_indices() {
        if i < open {
            continue;
        }
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close?;
    Some(split_arguments(&rest[open + 1..close]))
}

fn resolve_static_import_member(
    file: &FileSymbols,
    name: &str,
    is_call: bool,
    project: &ProjectTypes,
    index: Option<&ProjectIndex>,
    classpath: &ClasspathIndex,
) -> Option<ResolvedMember> {
    // JLS 7.5: a single-static-import shadows wildcard imports. Among several
    // single-static-imports, an unambiguous owner wins; distinct owners are
    // ambiguous (also reported by `static_import_conflicts`).
    let mut singles = Vec::new();
    let mut wildcards = Vec::new();
    for import in &file.imports {
        let Some(path) = import.trim().strip_prefix("static ") else {
            continue;
        };
        let Some((owner, member)) = path.rsplit_once('.') else {
            continue;
        };
        let member = member.trim();
        if member != "*" && member != name {
            continue;
        }
        let Some(owner_type) = resolve_type(owner, file, project, classpath) else {
            continue;
        };
        if member == "*" {
            wildcards.push(owner_type);
        } else {
            singles.push(owner_type);
        }
    }
    let mut owners: Vec<ResolvedType> = Vec::new();
    for owner in singles {
        if !owners.iter().any(|known| known.name() == owner.name()) {
            owners.push(owner);
        }
    }
    if owners.len() > 1 {
        return None;
    }
    if let Some(owner) = owners.pop() {
        return resolve_member_of_type(&owner, name, Some(is_call), project, index, classpath);
    }
    let mut found: Option<ResolvedMember> = None;
    for owner in wildcards {
        let Some(candidate) =
            resolve_member_of_type(&owner, name, Some(is_call), project, index, classpath)
        else {
            continue;
        };
        if let Some(existing) = &found {
            if existing.owner.name() != candidate.owner.name() {
                // Ambiguous wildcard imports: Java would reject the call and
                // a guess could rename the wrong member.
                return None;
            }
        } else {
            found = Some(candidate);
        }
    }
    found
}

/// Static-import conflicts for diagnostics: unqualified member references that
/// resolve to two or more distinct owners through static imports. Returns
/// `(name, line, owners)`, deduplicated by name.
pub fn static_import_conflicts(
    file: &FileSymbols,
    source: &str,
    project: &ProjectTypes,
    index: Option<&ProjectIndex>,
    classpath: &ClasspathIndex,
) -> Vec<(String, usize, Vec<String>)> {
    use std::collections::BTreeMap;
    let mut by_name: BTreeMap<String, (usize, Vec<String>)> = BTreeMap::new();
    let bindings = crate::parse_local_bindings(source);
    for reference in &file.member_refs {
        if reference.qualifier.is_some() {
            continue;
        }
        if local_binding_covers(&bindings, &reference.name, reference.start_byte) {
            continue;
        }
        let mut owners: Vec<String> = Vec::new();
        for import in &file.imports {
            let Some(path) = import.trim().strip_prefix("static ") else {
                continue;
            };
            let Some((owner, member)) = path.rsplit_once('.') else {
                continue;
            };
            let member = member.trim();
            if member != "*" && member != reference.name {
                continue;
            }
            let Some(owner_type) = resolve_type(owner, file, project, classpath) else {
                continue;
            };
            if resolve_member_of_type(
                &owner_type,
                &reference.name,
                Some(reference.is_call),
                project,
                index,
                classpath,
            )
            .is_some()
            {
                let owner_name = owner_type.name().to_owned();
                if !owners.contains(&owner_name) {
                    owners.push(owner_name);
                }
            }
        }
        if owners.len() > 1 {
            by_name
                .entry(reference.name.clone())
                .or_insert_with(|| (reference.line, Vec::new()));
            let entry = by_name.get_mut(&reference.name).unwrap();
            for owner in owners {
                if !entry.1.contains(&owner) {
                    entry.1.push(owner);
                }
            }
            entry.1.sort();
        }
    }
    by_name
        .into_iter()
        .map(|(name, (line, owners))| (name, line, owners))
        .collect()
}

/// Type variables visible in a file (class and method `<T ...>` declarations),
/// used to suppress false "unresolved type" positives for `T` usages.
pub fn file_type_variables(file: &FileSymbols) -> std::collections::HashSet<String> {
    let mut variables = std::collections::HashSet::new();
    for symbol in &file.symbols {
        let Some(detail) = symbol.detail.as_deref() else {
            continue;
        };
        let (params, _) = if symbol.kind.is_type() {
            // `class Box<T>`: params follow the type name.
            let pos = detail
                .find(&symbol.name)
                .map(|pos| pos + symbol.name.len())
                .unwrap_or(0);
            let rest = detail[pos..].trim_start();
            if !rest.starts_with('<') {
                (Vec::new(), false)
            } else {
                (parse_type_param_list_trimmed(rest), true)
            }
        } else if symbol.kind == SymbolKind::Method {
            // `<T> T identity`: params precede the method name.
            let pos = detail.rfind(&symbol.name).unwrap_or(detail.len());
            (type_params_of_detail(&detail[..pos]), true)
        } else {
            (Vec::new(), false)
        };
        for (name, _) in params {
            variables.insert(name);
        }
    }
    variables
}

fn is_java_primitive_or_void(name: &str) -> bool {
    matches!(
        name,
        "byte" | "short" | "int" | "long" | "float" | "double" | "char" | "boolean" | "void"
    )
}

/// Type references that do not resolve to any project or classpath type.
/// Type variables in scope, primitives and `var` are not reported.
pub fn unresolved_type_references(
    file: &FileSymbols,
    project: &ProjectTypes,
    classpath: &ClasspathIndex,
) -> Vec<(String, usize)> {
    let variables = file_type_variables(file);
    let mut seen = std::collections::HashSet::new();
    let mut problems = Vec::new();
    let mut check = |name: &str, line: usize| {
        let simple = name.rsplit('.').next().unwrap_or(name);
        if simple.is_empty()
            || is_java_primitive_or_void(simple)
            || simple == "var"
            || variables.contains(simple)
            || !seen.insert(simple.to_owned())
        {
            return;
        }
        if resolve_type(name, file, project, classpath).is_none()
            && resolve_type(simple, file, project, classpath).is_none()
        {
            problems.push((name.to_owned(), line));
        }
    };
    for reference in &file.type_refs {
        check(&reference.name, reference.line);
    }
    for reference in &file.scoped_type_refs {
        check(&reference.name, reference.line);
    }
    problems.sort_by(|left, right| left.1.cmp(&right.1).then(left.0.cmp(&right.0)));
    problems
}

/// Member references whose receiver resolves but which declare no such member
/// (e.g. typos like `user.nmae()`). Unqualified references that bind to a
/// local variable, `this`/`super` chains that fail, and array `length` are
/// skipped to avoid false positives.
pub fn unresolved_member_references(
    file: &FileSymbols,
    source: &str,
    project: &ProjectTypes,
    index: Option<&ProjectIndex>,
    classpath: &ClasspathIndex,
) -> Vec<(String, usize)> {
    let mut problems = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for reference in &file.member_refs {
        let Some(qualifier) = reference.qualifier.as_deref() else {
            continue;
        };
        if !seen.insert((
            reference.name.clone(),
            reference.start_byte,
            reference.is_call,
        )) {
            continue;
        }
        let cursor = char_index_of_byte(source, reference.start_byte);
        let Some(receiver) =
            resolve_expression_type(qualifier, source, cursor, file, project, index, classpath)
        else {
            continue;
        };
        // `arr.length`: arrays expose length but our model erases them to the
        // element type; never report length on an array variable.
        if !reference.is_call
            && reference.name == "length"
            && qualifier_is_plain_variable(qualifier)
            && variable_declared_as_array(source, cursor, qualifier)
        {
            continue;
        }
        let found = resolve_member_of_type_with_arity(
            &receiver,
            &reference.name,
            Some(reference.is_call),
            reference.argument_count,
            project,
            index,
            classpath,
        );
        if found.is_none() {
            problems.push((
                format!("{}.{}", receiver.name(), reference.name),
                reference.line,
            ));
        }
        if problems.len() >= 500 {
            break;
        }
    }
    problems.sort_by(|left, right| left.1.cmp(&right.1).then(left.0.cmp(&right.0)));
    problems
}

fn qualifier_is_plain_variable(qualifier: &str) -> bool {
    let qualifier = qualifier.trim();
    !qualifier.is_empty()
        && qualifier.chars().all(is_identifier_char)
        && qualifier != "this"
        && qualifier != "super"
}

/// True when the variable's declaration carries `[]` (`String[] arr`,
/// `new String[3]`, `void f(String[] args)`).
fn variable_declared_as_array(source: &str, cursor: usize, variable: &str) -> bool {
    let prefix: String = source.chars().take(cursor).collect();
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
        if before.contains('[') && before.contains(']') {
            return true;
        }
        if position == 0 {
            break;
        }
        search_end = position;
    }
    false
}

/// `@Override` methods with no matching supertype member.
pub fn override_problems(
    file: &FileSymbols,
    project: &ProjectTypes,
    index: Option<&ProjectIndex>,
    classpath: &ClasspathIndex,
) -> Vec<(String, usize)> {
    let Some(index) = index else {
        return Vec::new();
    };
    let mut problems = Vec::new();
    for symbol in &file.symbols {
        if symbol.kind != SymbolKind::Method {
            continue;
        }
        let Some(detail) = symbol.detail.as_deref() else {
            continue;
        };
        if !detail.contains("@Override") {
            continue;
        }
        let Some(container) = symbol.container.as_deref() else {
            continue;
        };
        let fqn = fqn_in_package(file.package.as_deref(), container);
        let mut supers = resolved_supertypes(&fqn, project, index, classpath);
        // Every class implicitly extends Object (e.g. `toString` overrides).
        if let Some(object) = resolve_fqn("java.lang.Object", project, classpath) {
            if !supers.iter().any(|ty| ty.name() == object.name()) {
                supers.push(object);
            }
        }
        if supers.is_empty() {
            // Unknown hierarchy (missing supertypes): stay silent instead of
            // reporting a false positive.
            continue;
        }
        let arity = detail_param_types(detail).map(|params| params.len());
        let found = supers.iter().any(|sup| {
            resolve_member_of_type_with_arity(
                sup,
                &symbol.name,
                Some(true),
                arity,
                project,
                Some(index),
                classpath,
            )
            .is_some()
        });
        if !found {
            problems.push((
                format!(
                    "{} não sobrescreve nenhum método da superclasse",
                    symbol.name
                ),
                symbol.line,
            ));
        }
        if problems.len() >= 200 {
            break;
        }
    }
    problems
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
    resolve_expression_generic(
        receiver,
        source,
        cursor,
        file,
        project,
        project_index,
        classpath,
    )
    .map(|generic| generic.ty)
}

fn resolve_expression_generic(
    receiver: &str,
    source: &str,
    cursor: usize,
    file: &FileSymbols,
    project: &ProjectTypes,
    project_index: Option<&ProjectIndex>,
    classpath: &ClasspathIndex,
) -> Option<ResolvedGeneric> {
    let segments: Vec<&str> = receiver
        .split('.')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    let first = *segments.first()?;
    let mut current = if first == "this" {
        let fqn = active_type_fqn_at(file, source, cursor)?;
        ResolvedGeneric::erased(resolve_fqn(&fqn, project, classpath)?)
    } else if first == "super" {
        let fqn = active_type_fqn_at(file, source, cursor)?;
        let owner = resolve_fqn(&fqn, project, classpath)?;
        ResolvedGeneric::erased(super_type_of(&owner, project, project_index, classpath)?)
    } else if let Some(name) = method_call_name(first) {
        let fqn = active_type_fqn_at(file, source, cursor)?;
        let owner = resolve_fqn(&fqn, project, classpath)?;
        let arg_types = segment_argument_types(
            first,
            source,
            cursor,
            file,
            project,
            project_index,
            classpath,
        );
        member_generic_type_with_args(
            &owner,
            name,
            project,
            project_index,
            classpath,
            segment_argument_count(first),
            arg_types.as_deref(),
            None,
        )?
    } else {
        match resolve_type(first, file, project, classpath) {
            Some(resolved) => ResolvedGeneric::erased(resolved),
            None => {
                let inferred = infer_variable_generic(
                    source,
                    cursor,
                    member_segment_name(first),
                    file,
                    project,
                    classpath,
                )?;
                inferred
            }
        }
    };
    for segment in &segments[1..] {
        let name = member_segment_name(segment);
        let arg_types = segment_argument_types(
            segment,
            source,
            cursor,
            file,
            project,
            project_index,
            classpath,
        );
        let receiver_args: Vec<ResolvedGeneric> = current.args.clone();
        let receiver_args = if receiver_args.is_empty() {
            None
        } else {
            Some(receiver_args)
        };
        current = member_generic_type_with_args(
            &current.ty,
            name,
            project,
            project_index,
            classpath,
            segment_argument_count(segment),
            arg_types.as_deref(),
            receiver_args.as_deref(),
        )?;
    }
    Some(current)
}

/// Infer argument simple types for a call segment like `save("x", count)`.
fn segment_argument_types(
    segment: &str,
    source: &str,
    cursor: usize,
    file: &FileSymbols,
    project: &ProjectTypes,
    project_index: Option<&ProjectIndex>,
    classpath: &ClasspathIndex,
) -> Option<Vec<Option<String>>> {
    let open = segment.find('(')?;
    let close = segment.rfind(')')?;
    if close <= open {
        return None;
    }
    let args = split_arguments(&segment[open + 1..close]);
    Some(
        args.into_iter()
            .map(|arg| {
                infer_arg_simple(
                    arg.trim(),
                    source,
                    cursor,
                    file,
                    project,
                    project_index,
                    classpath,
                )
            })
            .collect(),
    )
}

/// Infer a variable's generic type, preserving arguments when declared
/// (`List<User> users` -> `List` + [`User`]).
fn infer_variable_generic(
    source: &str,
    cursor: usize,
    variable: &str,
    file: &FileSymbols,
    project: &ProjectTypes,
    classpath: &ClasspathIndex,
) -> Option<ResolvedGeneric> {
    if variable.is_empty() {
        return None;
    }
    let prefix: String = source.chars().take(cursor).collect();
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
        if let Some(type_text) = generic_type_before_text(before) {
            if let Some(generic) = resolve_generic_text(&type_text, file, project, classpath) {
                return Some(generic);
            }
        }
        if ends_with_keyword(before, "var") {
            if let Some(type_name) = var_initializer_type(&prefix[end..]) {
                if let Some(ty) = resolve_type(&type_name, file, project, classpath) {
                    return Some(ResolvedGeneric::erased(ty));
                }
            }
        }
        if position == 0 {
            break;
        }
        search_end = position;
    }
    None
}

/// Extract a possibly-generic type before a variable name
/// (`private List<User> users` -> `List<User>`).
fn generic_type_before_text(text: &str) -> Option<String> {
    let mut text = text.trim_end();
    if text.ends_with(']') {
        let stripped = text.trim_end_matches(']').trim_end();
        let stripped = stripped.strip_suffix('[')?;
        text = stripped.trim_end();
    }
    // Walk back over an optional `<...>` block plus the base identifier.
    if text.ends_with('>') {
        let mut depth = 0i32;
        let mut open = None;
        for (i, c) in text.char_indices().rev() {
            match c {
                '>' => depth += 1,
                '<' => {
                    depth -= 1;
                    if depth == 0 {
                        open = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let open = open?;
        let base_start = text[..open]
            .char_indices()
            .rev()
            .take_while(|(_, c)| is_identifier_char(*c) || *c == '.')
            .last()
            .map(|(i, _)| i)?;
        let token = text[base_start..].trim().to_owned();
        // Include a trailing `[]` already handled above.
        if token.is_empty() {
            return None;
        }
        // Validate the base starts with an uppercase type name.
        let base_simple = token
            .split('<')
            .next()
            .unwrap_or(&token)
            .rsplit('.')
            .next()
            .unwrap_or(&token);
        if !base_simple.chars().next().is_some_and(|c| c.is_uppercase()) {
            return None;
        }
        return Some(token);
    }
    let start = text
        .char_indices()
        .rev()
        .take_while(|(_, character)| is_identifier_char(*character) || *character == '.')
        .last()
        .map(|(index, _)| index)?;
    let token = text[start..].trim_start_matches('.').to_owned();
    let simple = token.rsplit('.').next()?;
    if simple.is_empty()
        || !simple
            .chars()
            .next()
            .is_some_and(|character| character.is_uppercase())
    {
        return None;
    }
    Some(token)
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

fn segment_argument_count(segment: &str) -> Option<usize> {
    let open = segment.find('(')?;
    let close = segment.rfind(')')?;
    if close <= open {
        return None;
    }
    Some(count_arguments(&segment[open + 1..close]))
}

fn count_arguments(inner: &str) -> usize {
    split_arguments(inner).len()
}

/// Split a comma-separated argument list respecting nesting and strings.
fn split_arguments(inner: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();
    let mut chars = inner.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                current.push(c);
            }
            '"' if !in_single => {
                in_double = !in_double;
                current.push(c);
            }
            '\\' if in_single || in_double => {
                current.push(c);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            '<' | '(' | '[' if !in_single && !in_double => {
                depth += 1;
                current.push(c);
            }
            '>' | ')' | ']' if !in_single && !in_double => {
                depth = (depth - 1).max(0);
                current.push(c);
            }
            ',' if depth == 0 && !in_single && !in_double => {
                if !current.trim().is_empty() {
                    out.push(current.trim().to_owned());
                }
                current.clear();
            }
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_owned());
    }
    out
}

/// Infer the simple type of a call argument expression. Handles literals,
/// `new Type(...)`, variables (via reverse inference) and simple method chains.
/// Returns `None` when the type cannot be determined conservatively.
fn infer_arg_simple(
    expr: &str,
    source: &str,
    cursor: usize,
    file: &FileSymbols,
    project: &ProjectTypes,
    project_index: Option<&ProjectIndex>,
    classpath: &ClasspathIndex,
) -> Option<String> {
    let expr = expr.trim();
    if expr.is_empty() {
        return None;
    }
    if expr.starts_with('"') {
        return Some("String".to_owned());
    }
    if expr.starts_with('\'') && expr.len() >= 3 {
        return Some("char".to_owned());
    }
    if expr == "true" || expr == "false" {
        return Some("boolean".to_owned());
    }
    if expr == "null" {
        return Some("null".to_owned());
    }
    if let Some(rest) = expr.strip_prefix("new ") {
        let end = rest
            .char_indices()
            .take_while(|(_, c)| is_identifier_char(*c) || *c == '.')
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        let token = rest[..end].trim_start_matches('.');
        let simple = token.rsplit('.').next()?;
        if simple.chars().next().is_some_and(|c| c.is_uppercase()) {
            return Some(simple.to_owned());
        }
        return None;
    }
    if expr
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_digit() || (c == '-' && expr.len() > 1))
    {
        let lower = expr.to_lowercase();
        if lower.ends_with(['f', 'd']) || expr.contains('.') {
            return Some("double".to_owned());
        }
        if lower.ends_with('l') {
            return Some("long".to_owned());
        }
        return Some("int".to_owned());
    }
    if let Some(resolved) = resolve_expression_type(
        expr,
        source,
        cursor,
        file,
        project,
        project_index,
        classpath,
    ) {
        let simple = resolved
            .name()
            .rsplit('.')
            .next()
            .unwrap_or(resolved.name())
            .replace('$', ".");
        let simple = simple.rsplit('.').next().unwrap_or(&simple).to_owned();
        return Some(simple);
    }
    infer_variable_type(source, cursor, expr)
}

/// Parse `List<User>` into base `List` plus args [`User`], handling nesting.
fn parse_generic_parts(text: &str) -> (String, Vec<String>) {
    let text = text.trim();
    let Some(open) = text.find('<') else {
        return (text.to_owned(), Vec::new());
    };
    let mut depth = 0i32;
    let mut close = None;
    for (i, c) in text.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(close) = close else {
        return (text.to_owned(), Vec::new());
    };
    let base = text[..open].trim().to_owned();
    let inner = &text[open + 1..close];
    (base, split_arguments(inner))
}

fn parameter_count(detail: &str) -> Option<usize> {
    detail_param_types(detail).map(|params| params.len())
}

/// Parameter types from a source declaration detail
/// (`public User save(String name, int times)` -> [`String`, `int`]).
/// Generic arguments are stripped to the base simple name; varargs
/// (`String... args`) become `String[]`.
fn detail_param_types(detail: &str) -> Option<Vec<String>> {
    detail_param_list(detail).map(|(params, _)| params)
}

/// Parameter types plus a varargs flag for overload resolution.
fn detail_param_list(detail: &str) -> Option<(Vec<String>, bool)> {
    let open = detail.find('(')?;
    let close = detail[open..].find(')')? + open;
    let inner = detail[open + 1..close].trim();
    if inner.is_empty() {
        return Some((Vec::new(), false));
    }
    let mut params = Vec::new();
    let mut saw_varargs = false;
    for part in split_arguments(inner) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        // Drop annotations/modifiers; the type is everything before the last
        // identifier, keeping `...` for varargs.
        let mut tokens: Vec<&str> = part.split_whitespace().collect();
        if tokens.is_empty() {
            continue;
        }
        let mut name = tokens.pop().unwrap_or("");
        // `final String name` -> type tokens remain; `String... args` keeps `...`.
        if name == "..." && !tokens.is_empty() {
            name = tokens.pop().unwrap_or("");
        }
        let mut type_token = if tokens.is_empty() {
            // Single token like `String` without a name (abstract decl).
            name.trim_end_matches("...")
        } else {
            tokens.last().copied().unwrap_or(name)
        };
        // Handle `String... args` where the type token carries the ellipsis.
        let mut is_varargs = false;
        if let Some(stripped) = type_token.strip_suffix("...") {
            type_token = stripped;
            is_varargs = true;
        }
        if type_token.is_empty() {
            return None;
        }
        saw_varargs |= is_varargs;
        let base = type_token.split('<').next().unwrap_or(type_token).trim();
        let simple = base.rsplit('.').next().unwrap_or(base);
        if is_varargs {
            params.push(format!("{simple}[]"));
            continue;
        }
        if type_token.trim_end().ends_with("[]") {
            // Preserve array suffix already on the token.
            let suffix: String = type_token
                .chars()
                .rev()
                .take_while(|c| *c == '[' || *c == ']' || c.is_whitespace())
                .collect::<String>()
                .chars()
                .rev()
                .filter(|c| !c.is_whitespace())
                .collect();
            params.push(format!("{simple}{suffix}"));
        } else {
            params.push(simple.to_owned());
        }
    }
    let varargs = saw_varargs;
    Some((params, varargs))
}

/// Score fixed-arity application, returning `(phase, score)`.
/// Phase 0 is strict (exact, widening, subtyping); phase 1 allows boxing.
fn score_fixed_args(
    params: &[String],
    args: &[Option<String>],
    subtype: &dyn Fn(&str, &str) -> bool,
) -> Option<(u8, u32)> {
    if params.len() != args.len() {
        return None;
    }
    let mut phase = 0u8;
    let mut score = 0u32;
    for (param, arg) in params.iter().zip(args.iter()) {
        let Some(arg) = arg else {
            score += 1;
            continue;
        };
        if let Some(cost) = arg_conversion_cost(arg, param, 0, subtype) {
            score += cost;
        } else if let Some(cost) = arg_conversion_cost(arg, param, 1, subtype) {
            phase = 1;
            score += cost;
        } else {
            return None;
        }
    }
    Some((phase, score))
}

/// Single-argument conversion cost at the given phase, or `None`.
fn arg_conversion_cost(
    arg: &str,
    param: &str,
    phase: u8,
    subtype: &dyn Fn(&str, &str) -> bool,
) -> Option<u32> {
    if param == arg {
        return Some(0);
    }
    if arg == "null" {
        return if is_primitive(param.trim_end_matches("[]")) && !param.ends_with("[]") {
            None
        } else {
            Some(1)
        };
    }
    let param_base = param.trim_end_matches("[]");
    let arg_base = arg.trim_end_matches("[]");
    let param_array = param.ends_with("[]");
    let arg_array = arg.ends_with("[]");
    if param_array != arg_array {
        return None;
    }
    if param_base == "Object" && !is_primitive(arg_base) {
        return Some(2);
    }
    if !is_primitive(param_base) && !is_primitive(arg_base) {
        if subtype(arg_base, param_base) {
            return Some(if arg_base == param_base { 0 } else { 1 });
        }
        return None;
    }
    if phase == 0 {
        if is_primitive_widening(arg_base, param_base) {
            return Some(primitive_widening_cost(arg_base, param_base));
        }
        return None;
    }
    // Phase 1: boxing, unboxing, and unboxing followed by widening.
    if is_boxing_match(arg_base, param_base) {
        return Some(1);
    }
    if let Some(unboxed) = unbox(arg_base) {
        if unboxed == param_base {
            return Some(1);
        }
        if is_primitive_widening(unboxed, param_base) {
            return Some(2 + primitive_widening_cost(unboxed, param_base));
        }
    }
    if let Some(boxed) = box_for(arg_base) {
        if subtype(boxed, param_base) {
            return Some(2);
        }
    }
    None
}

/// Pick the winning overload index, or `None` when nothing applies.
/// Candidates are `(params, is_varargs)`. JLS order applies: fixed-arity
/// phases first, variable-arity last. Ties fall back to the first candidate
/// (best-effort navigation; rename updates every overload together).
fn pick_overload(
    candidates: &[(Vec<String>, bool)],
    args: &[Option<String>],
    subtype: &dyn Fn(&str, &str) -> bool,
) -> Option<usize> {
    let mut best: Option<(u8, u32, usize)> = None;
    for (index, (params, varargs)) in candidates.iter().enumerate() {
        // Fixed-arity reading (for varargs methods this uses the array type).
        if params.len() == args.len() {
            if let Some((phase, score)) = score_fixed_args(params, args, subtype) {
                if best.is_none_or(|(p, s, _)| (phase, score) < (p, s)) {
                    best = Some((phase, score, index));
                }
            }
        }
        if *varargs {
            if let Some(score) = score_varargs_call(params, args, subtype) {
                if best.is_none_or(|(p, s, _)| (2u8, score) < (p, s)) {
                    best = Some((2, score, index));
                }
            }
        }
    }
    best.map(|(_, _, index)| index)
}

/// Score a variable-arity call: fixed prefix plus one element check per extra
/// argument.
fn score_varargs_call(
    params: &[String],
    args: &[Option<String>],
    subtype: &dyn Fn(&str, &str) -> bool,
) -> Option<u32> {
    let (fixed, last) = params.split_at(params.len().saturating_sub(1));
    let element = last.last()?.trim_end_matches("[]");
    if args.len() < fixed.len() {
        return None;
    }
    let mut score = 0u32;
    for (param, arg) in fixed.iter().zip(args.iter()) {
        let Some(arg) = arg else {
            score += 1;
            continue;
        };
        if let Some(cost) = arg_conversion_cost(arg, param, 0, subtype)
            .or_else(|| arg_conversion_cost(arg, param, 1, subtype))
        {
            score += cost;
        } else {
            return None;
        }
    }
    for arg in &args[fixed.len()..] {
        let Some(arg) = arg else {
            score += 1;
            continue;
        };
        if let Some(cost) = arg_conversion_cost(arg, element, 0, subtype)
            .or_else(|| arg_conversion_cost(arg, element, 1, subtype))
            .map(|cost| cost + 1)
        {
            score += cost;
        } else {
            return None;
        }
    }
    Some(score)
}

/// True when `sub` is a subtype of `sup` (simple or fully-qualified names).
/// Walks project relations and classpath superclasses/interfaces transitively;
/// arrays are covariant, everything references `Object`.
fn is_subtype_simple(
    sub: &str,
    sup: &str,
    project: &ProjectTypes,
    index: Option<&ProjectIndex>,
    classpath: &ClasspathIndex,
) -> bool {
    let sub = sub.trim();
    let sup = sup.trim();
    if sub == sup {
        return true;
    }
    let (sub_base, sub_array) = strip_array(sub);
    let (sup_base, sup_array) = strip_array(sup);
    if is_primitive(sub_base) || is_primitive(sup_base) {
        return false;
    }
    if sup_base == "Object" {
        return true;
    }
    if sub_array != sup_array {
        // Covariance: `String[]` is a subtype of `Object[]`/`CharSequence[]`.
        if sub_array && sup_array {
            return sub_base == sup_base
                || is_subtype_simple(sub_base, sup_base, project, index, classpath);
        }
        return false;
    }
    // Resolve the subtype to qualified names and walk up.
    let mut qualified = Vec::new();
    if sub.contains('.') {
        qualified.push(sub.to_owned());
    }
    if let Some(names) = project.by_simple.get(sub) {
        qualified.extend(names.iter().cloned());
    }
    qualified.extend(classpath.simple_matches(sub).iter().cloned());
    if qualified.is_empty() {
        return false;
    }
    let mut visited = std::collections::HashSet::new();
    let mut stack = qualified;
    while let Some(fqn) = stack.pop() {
        if !visited.insert(fqn.clone()) {
            continue;
        }
        let simple = fqn.rsplit('.').next().unwrap_or(&fqn);
        if simple == sup_base || fqn == sup_base {
            return true;
        }
        if project.get(&fqn).is_some() {
            if let Some(index) = index {
                for parent in resolved_supertypes(&fqn, project, index, classpath) {
                    stack.push(parent.name().to_owned());
                }
            }
        } else if let Some(class) = classpath.get_loose(&fqn) {
            if let Some(parent) = &class.super_name {
                stack.push(parent.clone());
            }
            stack.extend(class.interfaces.iter().cloned());
        }
    }
    false
}

fn strip_array(name: &str) -> (&str, bool) {
    if let Some(base) = name.strip_suffix("[]") {
        (base.trim(), true)
    } else {
        (name, false)
    }
}

fn is_primitive(name: &str) -> bool {
    matches!(
        name,
        "byte" | "short" | "int" | "long" | "float" | "double" | "char" | "boolean" | "void"
    )
}

fn unbox(name: &str) -> Option<&'static str> {
    match name {
        "Integer" => Some("int"),
        "Long" => Some("long"),
        "Double" => Some("double"),
        "Float" => Some("float"),
        "Boolean" => Some("boolean"),
        "Character" => Some("char"),
        "Byte" => Some("byte"),
        "Short" => Some("short"),
        _ => None,
    }
}

fn box_for(name: &str) -> Option<&'static str> {
    match name {
        "int" => Some("Integer"),
        "long" => Some("Long"),
        "double" => Some("Double"),
        "float" => Some("Float"),
        "boolean" => Some("Boolean"),
        "char" => Some("Character"),
        "byte" => Some("Byte"),
        "short" => Some("Short"),
        _ => None,
    }
}

fn primitive_rank(name: &str) -> Option<u32> {
    Some(match name {
        "byte" => 1,
        "short" | "char" => 2,
        "int" => 3,
        "long" => 4,
        "float" => 5,
        "double" => 6,
        _ => return None,
    })
}

fn primitive_widening_cost(from: &str, to: &str) -> u32 {
    match (primitive_rank(from), primitive_rank(to)) {
        (Some(from), Some(to)) => to.saturating_sub(from).max(1),
        _ => 1,
    }
}

fn is_primitive_widening(from: &str, to: &str) -> bool {
    matches!(
        (from, to),
        ("byte", "short")
            | ("byte", "int")
            | ("byte", "long")
            | ("byte", "float")
            | ("byte", "double")
            | ("short", "int")
            | ("short", "long")
            | ("short", "float")
            | ("short", "double")
            | ("char", "int")
            | ("char", "long")
            | ("char", "float")
            | ("char", "double")
            | ("int", "long")
            | ("int", "float")
            | ("int", "double")
            | ("long", "float")
            | ("long", "double")
            | ("float", "double")
    )
}

fn is_boxing_match(from: &str, to: &str) -> bool {
    matches!(
        (from, to),
        ("int", "Integer")
            | ("Integer", "int")
            | ("long", "Long")
            | ("Long", "long")
            | ("double", "Double")
            | ("Double", "double")
            | ("float", "Float")
            | ("Float", "float")
            | ("boolean", "Boolean")
            | ("Boolean", "boolean")
            | ("char", "Character")
            | ("Character", "char")
            | ("byte", "Byte")
            | ("Byte", "byte")
            | ("short", "Short")
            | ("Short", "short")
    )
}

fn method_argument_count(descriptor: &str) -> Option<usize> {
    method_param_types(descriptor).map(|params| params.len())
}

/// Parameter types of a JVM method descriptor as simple names
/// (`java.lang.String` -> `String`). Type variables (`T`) are kept as-is.
fn method_param_types(descriptor: &str) -> Option<Vec<String>> {
    let bytes = descriptor.as_bytes();
    if bytes.first() != Some(&b'(') {
        return None;
    }
    let mut index = 1;
    let mut params = Vec::new();
    while index < bytes.len() && bytes[index] != b')' {
        match bytes[index] {
            b'[' => {
                let mut depth = 0;
                while bytes.get(index) == Some(&b'[') {
                    depth += 1;
                    index += 1;
                }
                let base = parse_single_type(bytes, &mut index)?;
                let mut name = simple_name(&base);
                for _ in 0..depth {
                    name.push_str("[]");
                }
                params.push(name);
            }
            _ => {
                let full = parse_single_type(bytes, &mut index)?;
                params.push(simple_name(&full));
            }
        }
    }
    Some(params)
}

fn parse_single_type(bytes: &[u8], index: &mut usize) -> Option<String> {
    match bytes.get(*index)? {
        b'B' => {
            *index += 1;
            Some("byte".to_owned())
        }
        b'C' => {
            *index += 1;
            Some("char".to_owned())
        }
        b'D' => {
            *index += 1;
            Some("double".to_owned())
        }
        b'F' => {
            *index += 1;
            Some("float".to_owned())
        }
        b'I' => {
            *index += 1;
            Some("int".to_owned())
        }
        b'J' => {
            *index += 1;
            Some("long".to_owned())
        }
        b'S' => {
            *index += 1;
            Some("short".to_owned())
        }
        b'Z' => {
            *index += 1;
            Some("boolean".to_owned())
        }
        b'V' => {
            *index += 1;
            Some("void".to_owned())
        }
        b'L' => {
            *index += 1;
            let start = *index;
            while *index < bytes.len() && bytes[*index] != b';' {
                *index += 1;
            }
            let name = String::from_utf8_lossy(&bytes[start..*index]).replace('/', ".");
            if bytes.get(*index) == Some(&b';') {
                *index += 1;
            }
            Some(name)
        }
        b'T' => {
            *index += 1;
            let start = *index;
            while *index < bytes.len() && bytes[*index] != b';' {
                *index += 1;
            }
            let name = String::from_utf8_lossy(&bytes[start..*index]).into_owned();
            if bytes.get(*index) == Some(&b';') {
                *index += 1;
            }
            Some(name)
        }
        _ => {
            *index += 1;
            None
        }
    }
}

fn simple_name(fqn: &str) -> String {
    let base = fqn.trim_end_matches("[]");
    let arrays = &fqn[base.len()..];
    let simple = base.rsplit('.').next().unwrap_or(base);
    format!("{simple}{arrays}")
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

#[derive(Debug, Clone)]
struct ResolvedGeneric {
    ty: ResolvedType,
    args: Vec<ResolvedGeneric>,
}

impl ResolvedGeneric {
    fn erased(ty: ResolvedType) -> Self {
        Self {
            ty,
            args: Vec::new(),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn member_generic_type_with_args(
    receiver: &ResolvedType,
    member: &str,
    project: &ProjectTypes,
    project_index: Option<&ProjectIndex>,
    classpath: &ClasspathIndex,
    argument_count: Option<usize>,
    argument_types: Option<&[Option<String>]>,
    receiver_args: Option<&[ResolvedGeneric]>,
) -> Option<ResolvedGeneric> {
    let resolved = resolve_member_of_type_with_args(
        receiver,
        member,
        None,
        argument_count,
        argument_types,
        project,
        project_index,
        classpath,
    )?;
    // Type-variable environment: method type params first (bound from the
    // call-site arguments when the parameter carries the variable), then owner
    // type params (bound from the receiver's type arguments), else bounds.
    let owner_params = owner_type_params(&resolved.owner, project, project_index, classpath);
    let mut env: Vec<(String, Option<ResolvedGeneric>)> = Vec::new();
    for (name, bound) in &owner_params {
        let value = receiver_args
            .and_then(|args| {
                owner_params
                    .iter()
                    .position(|(param, _)| param == name)
                    .and_then(|i| args.get(i).cloned())
            })
            .or_else(|| {
                bound.as_deref().and_then(|bound| {
                    resolve_generic_with_env(
                        bound,
                        &empty_file(),
                        project,
                        classpath,
                        &[],
                        &[],
                        None,
                    )
                })
            });
        push_env(&mut env, name.clone(), value);
    }
    // Hardcoded generic returns for common JDK containers when metadata is
    // erased (List.get -> E, Map.get -> V, ...).
    if let ResolvedType::Classpath(class) = &resolved.owner {
        if let Some(mapped) = classpath_generic_arg(class, &resolved.name, receiver_args) {
            return Some(mapped);
        }
    }
    match &resolved.owner {
        ResolvedType::Project(location) => {
            let index = project_index?;
            let file = index.files.get(&location.path)?;
            let method_params = method_type_params_of_member(&location.path, &resolved.name, index);
            let method_names: Vec<String> =
                method_params.iter().map(|(name, _)| name.clone()).collect();
            // Bind method type variables from inferred call-site arguments.
            for (name, bound) in &method_params {
                let value = bind_from_call_args(
                    name,
                    &resolved.param_generics,
                    argument_types,
                    file,
                    project,
                    classpath,
                )
                .or_else(|| {
                    bound.as_deref().and_then(|bound| {
                        resolve_generic_with_env(
                            bound,
                            file,
                            project,
                            classpath,
                            &method_names,
                            &env,
                            None,
                        )
                    })
                });
                push_env(&mut env, name.clone(), value);
            }
            // Preserve generic arguments from the source declaration when
            // available (e.g. `List<User>`); fall back to the erased name.
            let generic_text = declared_generic_text(&location.path, &resolved.name, index)
                .unwrap_or_else(|| resolved.declared_type.clone().unwrap_or_default());
            if generic_text.is_empty() {
                return None;
            }
            resolve_generic_with_env(
                &generic_text,
                file,
                project,
                classpath,
                &method_names,
                &env,
                None,
            )
        }
        ResolvedType::Classpath(_class) => {
            let method_params = resolved
                .generic_sig
                .as_deref()
                .map(signature_type_params)
                .unwrap_or_default();
            let method_names: Vec<String> =
                method_params.iter().map(|(name, _)| name.clone()).collect();
            for (name, bound) in &method_params {
                let value = bind_from_call_args(
                    name,
                    &resolved.param_generics,
                    argument_types,
                    &empty_file(),
                    project,
                    classpath,
                )
                .or_else(|| {
                    bound.as_deref().and_then(|bound| {
                        resolve_generic_with_env(
                            bound,
                            &empty_file(),
                            project,
                            classpath,
                            &method_names,
                            &env,
                            None,
                        )
                    })
                });
                push_env(&mut env, name.clone(), value);
            }
            let generic_text = resolved
                .generic_sig
                .as_deref()
                .and_then(signature_return_text)
                .or_else(|| resolved.declared_type.clone())?;
            if generic_text.is_empty() {
                return None;
            }
            resolve_generic_with_env(
                &generic_text,
                &empty_file(),
                project,
                classpath,
                &method_names,
                &env,
                None,
            )
            .or_else(|| {
                resolve_fqn(
                    &resolved.declared_type.clone().unwrap_or_default(),
                    project,
                    classpath,
                )
                .map(ResolvedGeneric::erased)
            })
        }
    }
}

fn push_env(
    env: &mut Vec<(String, Option<ResolvedGeneric>)>,
    name: String,
    value: Option<ResolvedGeneric>,
) {
    if let Some(slot) = env.iter_mut().find(|(existing, _)| *existing == name) {
        if slot.1.is_none() {
            slot.1 = value;
        }
    } else {
        env.push((name, value));
    }
}

/// Bind a method type variable from the inferred call-site argument whose
/// declared parameter type is exactly that variable (e.g. `<T> T identity(T
/// x)` called as `identity("hi")` binds `T` to `String`).
fn bind_from_call_args(
    var: &str,
    param_generics: &[String],
    argument_types: Option<&[Option<String>]>,
    file: &FileSymbols,
    project: &ProjectTypes,
    classpath: &ClasspathIndex,
) -> Option<ResolvedGeneric> {
    let args = argument_types?;
    for (generic_text, inferred) in param_generics.iter().zip(args.iter()) {
        // Varargs element types (`T...`/`T[]`) bind the same variable.
        let generic_base = generic_text
            .trim()
            .trim_end_matches("...")
            .trim_end_matches("[]")
            .trim();
        if generic_base != var {
            continue;
        }
        let Some(simple) = inferred else {
            continue;
        };
        // `null` carries no type information.
        if simple == "null" {
            continue;
        }
        if let Some(ty) = resolve_type(simple, file, project, classpath) {
            return Some(ResolvedGeneric::erased(ty));
        }
    }
    None
}

fn empty_file() -> FileSymbols {
    FileSymbols {
        path: PathBuf::new(),
        package: None,
        package_range: None,
        imports: Vec::new(),
        import_lines: Vec::new(),
        import_ranges: Vec::new(),
        symbols: Vec::new(),
        type_refs: Vec::new(),
        scoped_type_refs: Vec::new(),
        member_refs: Vec::new(),
        type_relations: Vec::new(),
        local_type_names: Vec::new(),
        test_methods: Vec::new(),
        errors: Vec::new(),
        line_count: 0,
        parse_error: None,
    }
}

fn declared_generic_text(path: &Path, member: &str, index: &ProjectIndex) -> Option<String> {
    let file = index.files.get(path)?;
    let symbol = file
        .symbols
        .iter()
        .find(|s| s.name == member && !s.kind.is_type())?;
    let detail = symbol.detail.as_deref()?;
    let pos = detail.rfind(&symbol.name)?;
    let before = detail[..pos].trim_end();
    generic_type_token_before(before)
}

/// Extract the trailing type token preserving `<...>` and `[]`
/// (`public List<User> find` -> `List<User>`).
fn generic_type_token_before(before: &str) -> Option<String> {
    let before = before.trim_end();
    if before.is_empty() {
        return None;
    }
    // If it ends with `>`, include the balanced `<...>` block plus base.
    if before.ends_with('>') {
        let mut depth = 0i32;
        let mut open = None;
        for (i, c) in before.char_indices().rev() {
            match c {
                '>' => depth += 1,
                '<' => {
                    depth -= 1;
                    if depth == 0 {
                        open = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let open = open?;
        let base_start = before[..open]
            .char_indices()
            .rev()
            .take_while(|(_, c)| is_identifier_char(*c) || *c == '.')
            .last()
            .map(|(i, _)| i)?;
        let token = before[base_start..].trim().to_owned();
        if token.is_empty() {
            return None;
        }
        return Some(token);
    }
    // Strip array suffix, keep it to re-append.
    let mut core = before;
    let mut suffix = String::new();
    if core.trim_end().ends_with(']') {
        let stripped = core.trim_end_matches(']').trim_end();
        let stripped = stripped.strip_suffix('[')?;
        suffix = "[]".to_owned();
        core = stripped.trim_end();
        // Multi-dimensional: loop once more is enough for tests.
        if core.ends_with(']') {
            return None;
        }
    }
    let start = core
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_identifier_char(*c) || *c == '.')
        .last()
        .map(|(i, _)| i)?;
    let mut token = core[start..].trim_start_matches('.').to_owned();
    if token.is_empty() {
        return None;
    }
    token.push_str(&suffix);
    Some(token)
}

/// Resolve `List<User>`-style text to a generic with resolved arguments.
/// Unknown arguments become erased placeholders so chaining stays total.
fn resolve_generic_text(
    text: &str,
    file: &FileSymbols,
    project: &ProjectTypes,
    classpath: &ClasspathIndex,
) -> Option<ResolvedGeneric> {
    resolve_generic_with_env(text, file, project, classpath, &[], &[], None)
}

/// Resolve possibly-generic type text, substituting type variables from
/// `method_params` (in scope first) and `env` (owner params, then bounds).
/// `env` maps variable names to their bound value (`None` = known but unbound).
fn resolve_generic_with_env(
    text: &str,
    file: &FileSymbols,
    project: &ProjectTypes,
    classpath: &ClasspathIndex,
    method_params: &[String],
    env: &[(String, Option<ResolvedGeneric>)],
    call_env: Option<&std::collections::HashMap<String, Option<ResolvedGeneric>>>,
) -> Option<ResolvedGeneric> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let (base, args) = parse_generic_parts(text);
    let base_stripped = base
        .rsplit('.')
        .next()
        .unwrap_or(&base)
        .trim()
        .trim_end_matches("[]")
        .trim();
    if base_stripped.is_empty() {
        return None;
    }
    // A bare type variable resolves through the env (method bindings first,
    // then owner bindings with bounds). Unknown names fall through to normal
    // type resolution.
    if !base_stripped.contains('.')
        && base_stripped
            .chars()
            .next()
            .is_some_and(|c| c.is_uppercase())
        && args.is_empty()
    {
        if let Some(call_env) = call_env {
            if let Some(Some(value)) = call_env.get(base_stripped) {
                return Some(value.clone());
            }
        }
        if let Some((_, bound)) = env.iter().find(|(name, _)| name == base_stripped) {
            if let Some(value) = bound.clone() {
                return Some(value);
            }
            return resolve_type("Object", file, project, classpath).map(ResolvedGeneric::erased);
        }
    }
    // Dotted base: prefer the fully-qualified name (Signatures are FQN).
    if base.contains('.') {
        let full = base.trim().trim_end_matches("[]");
        if let Some(ty) = resolve_fqn(full, project, classpath) {
            return Some(ResolvedGeneric {
                ty,
                args: resolve_generic_args(
                    &args,
                    file,
                    project,
                    classpath,
                    method_params,
                    env,
                    call_env,
                ),
            });
        }
    }
    let ty = resolve_type(base_stripped, file, project, classpath)?;
    // Re-run through the env in case the "type" itself was a variable that
    // resolved to a concrete generic (keeps nested args).
    Some(ResolvedGeneric {
        ty,
        args: resolve_generic_args(
            &args,
            file,
            project,
            classpath,
            method_params,
            env,
            call_env,
        ),
    })
}

/// Bound of a wildcard type argument: `?` -> `Object`,
/// `? extends B` / `? super B` -> `B`. Returns `None` for non-wildcards.
fn wildcard_bound(text: &str) -> Option<String> {
    let rest = text.trim().strip_prefix('?')?.trim();
    if rest.is_empty() {
        return Some("Object".to_owned());
    }
    let mut words = rest.split_whitespace();
    match words.next() {
        Some("extends") | Some("super") => {
            let bound: String = words.collect::<Vec<_>>().join(" ");
            Some(if bound.is_empty() {
                "Object".to_owned()
            } else {
                bound
            })
        }
        _ => None,
    }
}

fn resolve_generic_args(
    args: &[String],
    file: &FileSymbols,
    project: &ProjectTypes,
    classpath: &ClasspathIndex,
    method_params: &[String],
    env: &[(String, Option<ResolvedGeneric>)],
    call_env: Option<&std::collections::HashMap<String, Option<ResolvedGeneric>>>,
) -> Vec<ResolvedGeneric> {
    let mut resolved_args = Vec::new();
    for arg in args {
        // Wildcards (`?`, `? extends Number`, `? super Integer`) resolve to
        // their bound, or Object when unbounded.
        if let Some(bound) = wildcard_bound(arg) {
            if let Some(generic) = resolve_generic_with_env(
                &bound,
                file,
                project,
                classpath,
                method_params,
                env,
                call_env,
            ) {
                resolved_args.push(generic);
            } else if let Some(ty) = resolve_type("Object", file, project, classpath) {
                resolved_args.push(ResolvedGeneric::erased(ty));
            }
            continue;
        }
        let (arg_base, nested) = parse_generic_parts(arg);
        let arg_base_simple = arg_base
            .rsplit('.')
            .next()
            .unwrap_or(&arg_base)
            .trim()
            .trim_end_matches("[]");
        // Single uppercase letter: a type variable, not a class.
        if arg_base_simple.len() == 1
            && arg_base_simple.chars().all(|c| c.is_uppercase())
            && nested.is_empty()
        {
            if let Some(call_env) = call_env {
                if let Some(Some(value)) = call_env.get(arg_base_simple) {
                    resolved_args.push(value.clone());
                    continue;
                }
            }
            if let Some((_, bound)) = env.iter().find(|(name, _)| name == arg_base_simple) {
                if let Some(value) = bound.clone() {
                    resolved_args.push(value);
                } else if let Some(ty) = resolve_type("Object", file, project, classpath) {
                    resolved_args.push(ResolvedGeneric::erased(ty));
                }
                continue;
            }
        }
        if let Some(nested_generic) =
            resolve_generic_with_env(arg, file, project, classpath, method_params, env, call_env)
        {
            resolved_args.push(nested_generic);
        }
    }
    resolved_args
}

/// Parse the first balanced `<...>` group of type parameters in a declaration
/// detail, e.g. `class Box<T extends Number>` or `public <T> T identity`.
/// Returns `(name, bound)`; the bound is the first `&`-separated bound or None.
fn type_params_of_detail(detail: &str) -> Vec<(String, Option<String>)> {
    // Find the first `<` that starts a parameter section (preceded by a
    // non-identifier char, so `List<User>` return types are skipped).
    let bytes = detail.as_bytes();
    let mut start = None;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'<' && (i == 0 || !is_identifier_char(bytes[i - 1] as char)) {
            start = Some(i);
            break;
        }
    }
    let Some(start) = start else {
        return Vec::new();
    };
    let mut depth = 0i32;
    let mut end = None;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        match b {
            b'<' => depth += 1,
            b'>' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(end) = end else {
        return Vec::new();
    };
    // For methods the group must come before the method name; for types it
    // follows the type name. Accept both: only require balance.
    parse_type_param_list(&detail[start + 1..end])
}

fn parse_type_param_list(inner: &str) -> Vec<(String, Option<String>)> {
    let mut params = Vec::new();
    for part in split_arguments(inner) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        // `T`, `T extends Number`, `T extends Comparable<T> & Serializable`.
        let (name, bound) = match part.split_once("extends") {
            Some((name, bound)) => (
                name.trim().to_owned(),
                bound
                    .split('&')
                    .next()
                    .map(str::trim)
                    .filter(|bound| !bound.is_empty())
                    .map(str::to_owned),
            ),
            None => (part.to_owned(), None),
        };
        if name.is_empty() || name.contains(char::is_whitespace) {
            continue;
        }
        params.push((name, bound));
    }
    params
}

/// Type parameters declared by a type symbol (`class Box<T>`).
fn declared_type_params(symbol: &crate::JavaSymbol) -> Vec<(String, Option<String>)> {
    let Some(detail) = symbol.detail.as_deref() else {
        return Vec::new();
    };
    // The params follow the type name: `class Box<T ...>`.
    let pos = detail
        .find(&symbol.name)
        .map(|pos| pos + symbol.name.len())
        .unwrap_or(0);
    let rest = detail[pos..].trim_start();
    if !rest.starts_with('<') {
        return Vec::new();
    }
    parse_type_param_list_trimmed(rest)
}

/// Type parameters declared by a method (`<T> T identity`).
fn method_type_params_of_detail(detail: &str, name: &str) -> Vec<(String, Option<String>)> {
    let Some(pos) = detail.rfind(name) else {
        return Vec::new();
    };
    type_params_of_detail(&detail[..pos])
}

fn parse_type_param_list_trimmed(rest: &str) -> Vec<(String, Option<String>)> {
    let mut depth = 0i32;
    let mut end = None;
    for (i, c) in rest.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(end) = end else {
        return Vec::new();
    };
    parse_type_param_list(&rest[1..end])
}

/// Type parameters of the type that declares a member.
fn owner_type_params(
    owner: &ResolvedType,
    _project: &ProjectTypes,
    project_index: Option<&ProjectIndex>,
    classpath: &ClasspathIndex,
) -> Vec<(String, Option<String>)> {
    match owner {
        ResolvedType::Project(location) => {
            let Some(index) = project_index else {
                return Vec::new();
            };
            let Some(file) = index.files.get(&location.path) else {
                return Vec::new();
            };
            file.symbols
                .iter()
                .find(|symbol| {
                    symbol.kind.is_type() && crate::semantic::type_fqn(file, symbol) == location.fqn
                })
                .map(declared_type_params)
                .unwrap_or_default()
        }
        ResolvedType::Classpath(name) => classpath
            .get_loose(name)
            .and_then(|class| class.generic_signature.as_deref())
            .map(signature_type_params)
            .unwrap_or_default(),
    }
}

/// Method type parameters of a project member declaration.
fn method_type_params_of_member(
    path: &Path,
    member: &str,
    index: &ProjectIndex,
) -> Vec<(String, Option<String>)> {
    let Some(file) = index.files.get(path) else {
        return Vec::new();
    };
    file.symbols
        .iter()
        .find(|symbol| symbol.name == member && !symbol.kind.is_type())
        .and_then(|symbol| symbol.detail.as_deref())
        .map(|detail| method_type_params_of_detail(detail, member))
        .unwrap_or_default()
}

/// Parameter generic texts of a source method detail, e.g.
/// `save(String name, List<T> items)` -> [`String`, `List<T>`].
fn detail_param_generics(detail: &str) -> Vec<String> {
    let Some(open) = detail.find('(') else {
        return Vec::new();
    };
    let Some(close) = detail[open..].find(')').map(|offset| open + offset) else {
        return Vec::new();
    };
    let inner = detail[open + 1..close].trim();
    if inner.is_empty() {
        return Vec::new();
    }
    let mut params = Vec::new();
    for part in split_arguments(inner) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let mut tokens: Vec<&str> = part.split_whitespace().collect();
        if tokens.is_empty() {
            continue;
        }
        // Drop annotations and Java modifiers; the last token is the name.
        while tokens.len() > 1
            && (tokens[0].starts_with('@')
                || matches!(
                    tokens[0],
                    "public"
                        | "protected"
                        | "private"
                        | "static"
                        | "final"
                        | "abstract"
                        | "synchronized"
                        | "transient"
                        | "volatile"
                        | "default"
                        | "native"
                        | "strictfp"
                ))
        {
            tokens.remove(0);
        }
        if tokens.is_empty() {
            continue;
        }
        let mut type_token = if tokens.len() == 1 {
            tokens[0].trim_end_matches("...").to_owned()
        } else {
            tokens[..tokens.len() - 1].join(" ")
        };
        if type_token.ends_with("...") {
            type_token = format!("{}[]", type_token.trim_end_matches("...").trim());
        }
        if !type_token.is_empty() {
            params.push(type_token);
        }
    }
    params
}

/// Map common JDK generic methods to receiver type arguments.
fn classpath_generic_arg(
    class: &str,
    method: &str,
    receiver_args: Option<&[ResolvedGeneric]>,
) -> Option<ResolvedGeneric> {
    let args = receiver_args?;
    let simple = class.rsplit('.').next().unwrap_or(class);
    match (simple, method) {
        ("List", "get")
        | ("List", "iterator")
        | ("Set", "iterator")
        | ("Optional", "get")
        | ("Optional", "orElse")
        | ("Optional", "orElseThrow") => args.first().cloned(),
        ("Map", "get") | ("Map", "getOrDefault") => args.get(1).cloned(),
        ("Map", "keySet") | ("Map", "values") => args.first().cloned(),
        _ => None,
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
            generic_signature: None,
            kind: MemberKind::Method,
            is_static: false,
            is_public: true,
            is_varargs: false,
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
                generic_signature: None,
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
                generic_signature: None,
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
    fn resolves_overloads_by_argument_count() {
        let method = |name: &str, descriptor: &str, signature: &str| MemberInfo {
            name: name.to_owned(),
            descriptor: descriptor.to_owned(),
            signature: signature.to_owned(),
            generic_signature: None,
            kind: MemberKind::Method,
            is_static: false,
            is_public: true,
            is_varargs: false,
        };
        let mut classpath = ClasspathIndex::default();
        classpath.classes.insert(
            "com.acme.Factory".to_owned(),
            ClassInfo {
                name: "com.acme.Factory".to_owned(),
                display_name: "Factory".to_owned(),
                package: "com.acme".to_owned(),
                kind: TypeKind::Class,
                is_public: true,
                super_name: None,
                interfaces: Vec::new(),
                fields: Vec::new(),
                methods: vec![
                    method("make", "()Ljava/lang/String;", "String make()"),
                    method("make", "(I)Ljava/lang/Integer;", "Integer make(int)"),
                ],
                generic_signature: None,
                origin: "test".to_owned(),
            },
        );
        let project = ProjectTypes::default();
        let target = ResolvedType::Classpath("com.acme.Factory".to_owned());
        let no_args = resolve_member_of_type_with_arity(
            &target,
            "make",
            Some(true),
            Some(0),
            &project,
            None,
            &classpath,
        )
        .unwrap();
        assert_eq!(no_args.declared_type.as_deref(), Some("java.lang.String"));
        let one_arg = resolve_member_of_type_with_arity(
            &target,
            "make",
            Some(true),
            Some(1),
            &project,
            None,
            &classpath,
        )
        .unwrap();
        assert_eq!(one_arg.declared_type.as_deref(), Some("java.lang.Integer"));
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
    fn overloads_prefer_matching_argument_types() {
        let (index, types) = project_with_files(vec![
            (
                PathBuf::from("com/acme/User.java"),
                "package com.acme; public class User { public void save(String name) {} public void save(int times) {} }",
            ),
            (
                PathBuf::from("com/acme/App.java"),
                "package com.acme; class App { void run(User user) { user.save(\"x\"); user.save(2); user.save(user); } }",
            ),
        ]);
        let file = index
            .files
            .get(&PathBuf::from("com/acme/App.java"))
            .unwrap();
        let source = "package com.acme; class App { void run(User user) { user.save(\"x\"); } }";
        let cursor = source.find("user.save").unwrap() + "user.".len();
        let target = member_target_at(
            source,
            file,
            source[..cursor].chars().count(),
            &types,
            &index,
            &ClasspathIndex::default(),
        )
        .unwrap();
        // Type-based scoring still resolves to the declaring type; arity
        // distinguishes the overloads during reference search below.
        assert_eq!(target.owner_name(), "com.acme.User");
        let owner = ResolvedType::Project(types.get("com.acme.User").unwrap().clone());
        let string_call = resolve_member_of_type_with_args(
            &owner,
            "save",
            Some(true),
            Some(1),
            Some(&[Some("String".to_owned())]),
            &types,
            Some(&index),
            &ClasspathIndex::default(),
        )
        .unwrap();
        assert!(string_call.detail.contains("String"));
        let int_call = resolve_member_of_type_with_args(
            &owner,
            "save",
            Some(true),
            Some(1),
            Some(&[Some("int".to_owned())]),
            &types,
            Some(&index),
            &ClasspathIndex::default(),
        )
        .unwrap();
        assert!(int_call.detail.contains("int"));
    }

    #[test]
    fn completes_generic_chained_returns() {
        use reclipse_classpath::{ClassInfo, MemberInfo, MemberKind, TypeKind};
        let (index, types) = project_with_files(vec![
            (
                PathBuf::from("com/acme/User.java"),
                "package com.acme; public class User { public String displayName() { return \"\"; } }",
            ),
            (
                PathBuf::from("com/acme/Repo.java"),
                "package com.acme; import java.util.List; public class Repo { public List<User> findAll() { return null; } }",
            ),
        ]);
        // Minimal java.util.List stand-in so the test is hermetic (no JDK needed).
        let mut classpath = ClasspathIndex::default();
        classpath.classes.insert(
            "java.util.List".to_owned(),
            ClassInfo {
                name: "java.util.List".to_owned(),
                display_name: "List".to_owned(),
                package: "java.util".to_owned(),
                kind: TypeKind::Interface,
                is_public: true,
                super_name: Some("java.lang.Object".to_owned()),
                interfaces: Vec::new(),
                fields: Vec::new(),
                methods: vec![
                    MemberInfo {
                        name: "get".to_owned(),
                        descriptor: "(I)Ljava/lang/Object;".to_owned(),
                        signature: "Object get(int)".to_owned(),
                        generic_signature: None,
                        kind: MemberKind::Method,
                        is_static: false,
                        is_public: true,
                        is_varargs: false,
                    },
                    MemberInfo {
                        name: "size".to_owned(),
                        descriptor: "()I".to_owned(),
                        signature: "int size()".to_owned(),
                        generic_signature: None,
                        kind: MemberKind::Method,
                        is_static: false,
                        is_public: true,
                        is_varargs: false,
                    },
                ],
                generic_signature: None,
                origin: "test".to_owned(),
            },
        );
        classpath
            .by_simple
            .insert("List".to_owned(), vec!["java.util.List".to_owned()]);
        classpath.packages.insert("java.util".to_owned());
        let app = empty_file("com/acme/App.java", Some("com.acme"));
        // findAll() returns List<User>: completion shows List members.
        let source = "Repo repo = new Repo();\nrepo.findAll().";
        let context = completion_context(
            source,
            source.chars().count(),
            &app,
            &types,
            Some(&index),
            &classpath,
        );
        assert!(context.receiver.as_deref() == Some("repo.findAll()"));
        assert!(context.items.iter().any(|item| item.label == "get"));
        assert!(context.items.iter().any(|item| item.label == "size"));
        // Chained through the generic element: get(0) yields User.
        let chained = resolve_expression_type(
            "repo.findAll().get(0)",
            source,
            source.chars().count(),
            &app,
            &types,
            Some(&index),
            &classpath,
        );
        assert_eq!(chained.as_ref().map(|ty| ty.name()), Some("com.acme.User"));
        let context2_source = "Repo repo = new Repo();\nrepo.findAll().get(0).";
        let context2 = completion_context(
            context2_source,
            context2_source.chars().count(),
            &app,
            &types,
            Some(&index),
            &classpath,
        );
        assert!(context2
            .items
            .iter()
            .any(|item| item.label == "displayName"));
        let (base, args) = parse_generic_parts("List<User>");
        assert_eq!(base, "List");
        assert_eq!(args, vec!["User".to_owned()]);
        let file2 = empty_file("com/acme/App2.java", Some("com.acme"));
        let generic_src = "List<User> users;\nusers.";
        assert!(infer_variable_generic(
            generic_src,
            generic_src.chars().count(),
            "users",
            &file2,
            &types,
            &classpath
        )
        .is_some());
    }

    #[test]
    fn substitutes_project_type_variables() {
        let (index, types) = project_with_files(vec![
            (
                PathBuf::from("com/acme/User.java"),
                "package com.acme; public class User { public String displayName() { return \"\"; } }",
            ),
            (
                PathBuf::from("com/acme/Box.java"),
                "package com.acme; public class Box<T> { public T get() { return null; } }",
            ),
        ]);
        let classpath = string_classpath();
        let app = empty_file("com/acme/App.java", Some("com.acme"));
        // Box<String>.get() resolves to String through the type variable.
        let chained = resolve_expression_type(
            "box.get()",
            "Box<String> box;\nbox.get()",
            "Box<String> box;\nbox.get()".chars().count(),
            &app,
            &types,
            Some(&index),
            &classpath,
        );
        assert_eq!(
            chained.as_ref().map(|ty| ty.name()),
            Some("java.lang.String")
        );
        // Unbound Box.get() falls back to the Object bound, not a dead end.
        let unbound = resolve_expression_type(
            "box.get()",
            "Box box;\nbox.get()",
            "Box box;\nbox.get()".chars().count(),
            &app,
            &types,
            Some(&index),
            &classpath,
        );
        assert_eq!(
            unbound.as_ref().map(|ty| ty.name()),
            Some("java.lang.Object")
        );
    }

    #[test]
    fn binds_generic_method_from_call_args() {
        let (index, types) = project_with_files(vec![(
            PathBuf::from("com/acme/Util.java"),
            "package com.acme; public class Util { public static <T> T identity(T x) { return x; } }",
        )]);
        let classpath = string_classpath();
        let app = empty_file("com/acme/App.java", Some("com.acme"));
        let source = "Util.identity(\"hi\").";
        let context = completion_context(
            source,
            source.chars().count(),
            &app,
            &types,
            Some(&index),
            &classpath,
        );
        assert!(context.receiver.as_deref() == Some("Util.identity(\"hi\")"));
        // T binds to String from the call argument: String members offered.
        assert!(context.items.iter().any(|item| item.label == "length"));
    }

    #[test]
    fn substitutes_classpath_signature_type_variables() {
        let mut classpath = ClasspathIndex::default();
        classpath.classes.insert(
            "com.acme.Box".to_owned(),
            ClassInfo {
                name: "com.acme.Box".to_owned(),
                display_name: "Box".to_owned(),
                package: "com.acme".to_owned(),
                kind: TypeKind::Class,
                is_public: true,
                super_name: Some("java.lang.Object".to_owned()),
                interfaces: Vec::new(),
                fields: Vec::new(),
                methods: vec![MemberInfo {
                    name: "get".to_owned(),
                    descriptor: "()Ljava/lang/Object;".to_owned(),
                    signature: "Object get()".to_owned(),
                    generic_signature: Some("()TT;".to_owned()),
                    kind: MemberKind::Method,
                    is_static: false,
                    is_public: true,
                    is_varargs: false,
                }],
                generic_signature: Some("<T::Ljava/lang/Object;>Ljava/lang/Object;".to_owned()),
                origin: "test".to_owned(),
            },
        );
        classpath
            .by_simple
            .insert("Box".to_owned(), vec!["com.acme.Box".to_owned()]);
        classpath.packages.insert("com.acme".to_owned());
        // Simulate `Box<String>`: receiver args carry the bound type.
        let receiver = ResolvedType::Classpath("com.acme.Box".to_owned());
        let string_ty = ResolvedType::Classpath("java.lang.String".to_owned());
        let args = vec![ResolvedGeneric::erased(string_ty)];
        let member = resolve_member_of_type_with_args(
            &receiver,
            "get",
            Some(true),
            Some(0),
            Some(&[]),
            &ProjectTypes::default(),
            None,
            &classpath,
        )
        .unwrap();
        assert_eq!(member.name, "get");
        let generic = member_generic_type_with_args(
            &receiver,
            "get",
            &ProjectTypes::default(),
            None,
            &classpath,
            Some(0),
            Some(&[]),
            Some(&args),
        )
        .unwrap();
        assert_eq!(generic.ty.name(), "java.lang.String");
    }

    fn static_import_project(files: Vec<(PathBuf, &str)>) -> (ProjectIndex, ProjectTypes) {
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
            all_files: Vec::new(),
        };
        let index = ProjectIndex::from_files(project, parsed, 0);
        let types = ProjectTypes::build(&index);
        (index, types)
    }

    #[test]
    fn single_static_import_shadows_wildcard() {
        let (index, types) = static_import_project(vec![
            (
                PathBuf::from("com/acme/A.java"),
                "package com.acme; public class A { public static void save() {} }",
            ),
            (
                PathBuf::from("com/acme/B.java"),
                "package com.acme; public class B { public static void save() {} }",
            ),
            (
                PathBuf::from("com/acme/App.java"),
                "package com.acme; import static com.acme.A.save; import static com.acme.B.*; class App { void run() { save(); } }",
            ),
        ]);
        let app = index
            .files
            .get(&PathBuf::from("com/acme/App.java"))
            .unwrap();
        let source = "package com.acme; import static com.acme.A.save; import static com.acme.B.*; class App { void run() { save(); } }";
        let cursor = source.find("save();").unwrap();
        let target = member_target_at(
            source,
            app,
            source[..cursor].chars().count(),
            &types,
            &index,
            &ClasspathIndex::default(),
        )
        .unwrap();
        assert_eq!(target.owner_name(), "com.acme.A");
    }

    #[test]
    fn conflicting_static_imports_are_reported() {
        let (index, types) = static_import_project(vec![
            (
                PathBuf::from("com/acme/A.java"),
                "package com.acme; public class A { public static void save() {} }",
            ),
            (
                PathBuf::from("com/acme/B.java"),
                "package com.acme; public class B { public static void save() {} }",
            ),
            (
                PathBuf::from("com/acme/App.java"),
                "package com.acme; import static com.acme.A.save; import static com.acme.B.save; class App { void run() { save(); } }",
            ),
        ]);
        let app = index
            .files
            .get(&PathBuf::from("com/acme/App.java"))
            .unwrap();
        let source = "package com.acme; import static com.acme.A.save; import static com.acme.B.save; class App { void run() { save(); } }";
        let conflicts = static_import_conflicts(
            app,
            source,
            &types,
            Some(&index),
            &ClasspathIndex::default(),
        );
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].0, "save");
        assert!(conflicts[0].2.contains(&"com.acme.A".to_owned()));
        assert!(conflicts[0].2.contains(&"com.acme.B".to_owned()));
    }

    #[test]
    fn reports_unresolved_types_but_not_type_variables() {
        let (index, types) = project_with_files(vec![(
            PathBuf::from("com/acme/Box.java"),
            "package com.acme; public class Box<T> { T value; Missing other; var x = 1; }",
        )]);
        let file = index
            .files
            .get(&PathBuf::from("com/acme/Box.java"))
            .unwrap();
        let problems = unresolved_type_references(file, &types, &ClasspathIndex::default());
        assert!(problems.iter().any(|(name, _)| name == "Missing"));
        assert!(!problems.iter().any(|(name, _)| name == "T"));
        assert!(!problems.iter().any(|(name, _)| name == "var"));
        assert!(!problems
            .iter()
            .any(|(name, _)| name == "String" || name == "Box"));
    }

    #[test]
    fn reports_unresolved_members_but_not_array_length() {
        let (index, types) = project_with_files(vec![
            (
                PathBuf::from("com/acme/User.java"),
                "package com.acme; public class User { public String name() { return \"\"; } }",
            ),
            (
                PathBuf::from("com/acme/App.java"),
                "package com.acme; class App { void run(User user, String[] args) { user.nmae(); int n = args.length; } }",
            ),
        ]);
        let file = index
            .files
            .get(&PathBuf::from("com/acme/App.java"))
            .unwrap();
        let source = "package com.acme; class App { void run(User user, String[] args) { user.nmae(); int n = args.length; } }";
        let problems = unresolved_member_references(
            file,
            source,
            &types,
            Some(&index),
            &ClasspathIndex::default(),
        );
        assert!(problems.iter().any(|(message, _)| message.contains("nmae")));
        assert!(!problems
            .iter()
            .any(|(message, _)| message.contains("length")));
    }

    #[test]
    fn validates_override_annotations() {
        let (index, types) = project_with_files(vec![
            (
                PathBuf::from("com/acme/Base.java"),
                "package com.acme; public class Base { public void save() {} }",
            ),
            (
                PathBuf::from("com/acme/Sub.java"),
                "package com.acme; public class Sub extends Base { @Override public void save() {} }",
            ),
            (
                PathBuf::from("com/acme/Broken.java"),
                "package com.acme; public class Broken extends Base { @Override public void missing() {} }",
            ),
        ]);
        let sub = index
            .files
            .get(&PathBuf::from("com/acme/Sub.java"))
            .unwrap();
        assert!(
            override_problems(sub, &types, Some(&index), &ClasspathIndex::default()).is_empty()
        );
        let broken = index
            .files
            .get(&PathBuf::from("com/acme/Broken.java"))
            .unwrap();
        let problems = override_problems(broken, &types, Some(&index), &ClasspathIndex::default());
        assert_eq!(problems.len(), 1);
        assert!(problems[0].0.contains("missing"));
    }

    #[test]
    fn overloads_prefer_subtypes_and_varargs() {
        let (index, types) = project_with_files(vec![
            (
                PathBuf::from("com/acme/Base.java"),
                "package com.acme; public class Base {}",
            ),
            (
                PathBuf::from("com/acme/Sub.java"),
                "package com.acme; public class Sub extends Base { public void show(String s) {} public void show(Object o) {} public void take(String... items) {} public void count(Integer n) {} }",
            ),
        ]);
        let no_index: Option<&ProjectIndex> = Some(&index);
        let classpath = ClasspathIndex::default();
        let subtype =
            |sub: &str, sup: &str| is_subtype_simple(sub, sup, &types, no_index, &classpath);
        assert!(subtype("Sub", "Base"));
        assert!(subtype("Sub", "Object"));
        assert!(!subtype("Base", "Sub"));
        assert!(subtype("String[]", "Object[]"));
        assert!(!subtype("int", "long"));
        // Most specific overload wins (String over Object).
        let cands = vec![
            (vec!["Object".to_owned()], false),
            (vec!["String".to_owned()], false),
        ];
        assert_eq!(
            pick_overload(&cands, &[Some("String".to_owned())], &subtype),
            Some(1)
        );
        // Boxing is a later phase than widening: int arg prefers long over Integer.
        let cands = vec![
            (vec!["Integer".to_owned()], false),
            (vec!["long".to_owned()], false),
        ];
        assert_eq!(
            pick_overload(&cands, &[Some("int".to_owned())], &subtype),
            Some(1)
        );
        // Integer arg matches Integer exactly, not int (no unboxing needed... both
        // applicable, exact wins).
        let cands = vec![
            (vec!["int".to_owned()], false),
            (vec!["Integer".to_owned()], false),
        ];
        assert_eq!(
            pick_overload(&cands, &[Some("Integer".to_owned())], &subtype),
            Some(1)
        );
        // Varargs: zero, one and many args all apply.
        let cands = vec![(vec!["String[]".to_owned()], true)];
        assert_eq!(pick_overload(&cands, &[], &subtype), Some(0));
        assert_eq!(
            pick_overload(&cands, &[Some("String".to_owned())], &subtype),
            Some(0)
        );
        assert_eq!(
            pick_overload(
                &cands,
                &[Some("String".to_owned()), Some("String".to_owned())],
                &subtype
            ),
            Some(0)
        );
        // Fixed arity beats varargs for a single arg (JLS phases 1-2 before 3).
        let cands = vec![
            (vec!["String[]".to_owned()], true),
            (vec!["String".to_owned()], false),
        ];
        assert_eq!(
            pick_overload(&cands, &[Some("String".to_owned())], &subtype),
            Some(1)
        );
        // Arity mismatch still rejects.
        let cands = vec![(vec!["String".to_owned(), "int".to_owned()], false)];
        assert_eq!(
            pick_overload(&cands, &[Some("String".to_owned())], &subtype),
            None
        );
        // Numeric widening prefers the most specific primitive.
        let cands = vec![
            (vec!["long".to_owned()], false),
            (vec!["int".to_owned()], false),
        ];
        assert_eq!(
            pick_overload(&cands, &[Some("byte".to_owned())], &subtype),
            Some(1)
        );
        let cands = vec![
            (vec!["float".to_owned()], false),
            (vec!["long".to_owned()], false),
        ];
        assert_eq!(
            pick_overload(&cands, &[Some("int".to_owned())], &subtype),
            Some(1)
        );
        // null is not applicable to primitives.
        let cands = vec![
            (vec!["int".to_owned()], false),
            (vec!["String".to_owned()], false),
        ];
        assert_eq!(
            pick_overload(&cands, &[Some("null".to_owned())], &subtype),
            Some(1)
        );
        // detail_param_list reports varargs.
        assert_eq!(
            detail_param_list("void take(String... items)"),
            Some((vec!["String[]".to_owned()], true))
        );
        assert_eq!(
            detail_param_list("void save(String name, int times)"),
            Some((vec!["String".to_owned(), "int".to_owned()], false))
        );
    }

    #[test]
    fn wildcards_resolve_to_their_bounds() {
        let (index, types) = project_with_files(vec![(
            PathBuf::from("com/acme/App.java"),
            "package com.acme; class App {}",
        )]);
        let file = index
            .files
            .get(&PathBuf::from("com/acme/App.java"))
            .unwrap();
        let mut classpath = string_classpath();
        classpath
            .by_simple
            .insert("List".to_owned(), vec!["java.util.List".to_owned()]);
        classpath.packages.insert("java.util".to_owned());
        classpath.type_names.push("java.util.List".to_owned());
        let resolved = resolve_generic_with_env(
            "List<? extends Number>",
            file,
            &types,
            &classpath,
            &[],
            &[],
            None,
        )
        .unwrap();
        assert_eq!(resolved.ty.name(), "java.util.List");
        // `Number` itself is not indexed in the fake classpath, so the bound
        // falls back to the erased placeholder... instead check Object bound:
        let object_list =
            resolve_generic_with_env("List<?>", file, &types, &classpath, &[], &[], None).unwrap();
        assert_eq!(object_list.ty.name(), "java.util.List");
        assert_eq!(object_list.args.len(), 1);
        assert_eq!(object_list.args[0].ty.name(), "java.lang.Object");
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
