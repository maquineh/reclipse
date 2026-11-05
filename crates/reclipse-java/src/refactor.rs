//! Conservative rename planning over the syntax/semantic index.
//!
//! Rename never guesses: unresolved or ambiguous references are left untouched,
//! shadowed locals abort the operation, and classpath members are read-only.
//! Javadoc, strings and reflection are not rewritten (reported as warnings).

use std::fs;
use std::path::{Path, PathBuf};

use reclipse_classpath::ClasspathIndex;
use tree_sitter::{Node, Parser};

use crate::semantic::{
    find_member_references, find_type_references, resolve_member_of_type, supertypes_of,
    MemberTarget, ProjectTypes, ResolvedType,
};
use crate::{parse_local_bindings, LocalBinding, ProjectIndex, SymbolKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub start_byte: usize,
    pub end_byte: usize,
    pub replacement: String,
}

#[derive(Debug, Clone)]
pub struct FileEdits {
    pub path: PathBuf,
    pub edits: Vec<TextEdit>,
}

impl FileEdits {
    pub fn len(&self) -> usize {
        self.edits.len()
    }

    pub fn is_empty(&self) -> bool {
        self.edits.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameKind {
    Type,
    Method,
    Field,
    Local,
}

impl RenameKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Type => "tipo",
            Self::Method => "método",
            Self::Field => "campo",
            Self::Local => "variável local",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RenamePlan {
    pub old_name: String,
    pub new_name: String,
    pub kind: RenameKind,
    pub files: Vec<FileEdits>,
    /// `(from, to)` when a top-level type also renames its `.java` file.
    pub rename_file: Option<(PathBuf, PathBuf)>,
    pub warnings: Vec<String>,
}

impl RenamePlan {
    pub fn total_edits(&self) -> usize {
        self.files.iter().map(FileEdits::len).sum()
    }
}

#[derive(Debug, Clone)]
pub struct LocalRenamePlan {
    pub old_name: String,
    pub new_name: String,
    pub edits: Vec<TextEdit>,
}

/// Plan a type rename: declaration, semantic type references and imports.
pub fn plan_type_rename(
    fqn: &str,
    new_name: &str,
    project: &ProjectTypes,
    index: &ProjectIndex,
    classpath: &ClasspathIndex,
) -> Result<RenamePlan, String> {
    validate_identifier(new_name)?;
    let location = project
        .get(fqn)
        .ok_or_else(|| "o tipo não pertence ao projeto (classpath é somente leitura)".to_owned())?;
    let old_name = location
        .fqn
        .rsplit('.')
        .next()
        .unwrap_or(&location.fqn)
        .to_owned();
    if old_name == new_name {
        return Err("o novo nome é igual ao atual".to_owned());
    }
    let package = location
        .fqn
        .rsplit_once('.')
        .map(|(package, _)| package.to_owned())
        .unwrap_or_default();
    let new_fqn = if package.is_empty() {
        new_name.to_owned()
    } else {
        format!("{package}.{new_name}")
    };
    if project.get(&new_fqn).is_some() {
        return Err(format!("já existe o tipo {new_fqn} no projeto"));
    }
    if classpath.get_loose(&new_fqn).is_some() {
        return Err(format!("já existe o tipo {new_fqn} no classpath"));
    }

    let mut files: Vec<FileEdits> = Vec::new();
    for (path, file) in &index.files {
        for symbol in &file.symbols {
            if symbol.kind.is_type() && crate::semantic::type_fqn(file, symbol) == fqn {
                push_edit(
                    &mut files,
                    path,
                    symbol.name_start_byte,
                    symbol.name_end_byte,
                    new_name,
                );
            }
        }
        if path == &location.path {
            let package = file.package.as_deref().unwrap_or("");
            let container = location
                .fqn
                .strip_prefix(package)
                .unwrap_or(&location.fqn)
                .trim_start_matches('.');
            for symbol in &file.symbols {
                if symbol.kind == SymbolKind::Constructor
                    && symbol.name == old_name
                    && symbol.container.as_deref() == Some(container)
                {
                    push_edit(
                        &mut files,
                        path,
                        symbol.name_start_byte,
                        symbol.name_end_byte,
                        new_name,
                    );
                }
            }
        }
    }
    for reference in find_type_references(fqn, project, index, classpath) {
        push_edit(
            &mut files,
            &reference.file,
            reference.start_byte,
            reference.end_byte,
            new_name,
        );
    }
    for (path, file) in &index.files {
        let relevant = file.imports.iter().any(|import| {
            if !import.contains(&old_name) {
                return false;
            }
            if let Some(owner) = static_import_owner(import) {
                owner == fqn
            } else if import.ends_with(".*") {
                false
            } else {
                import.strip_prefix("static ").unwrap_or(import).trim() == fqn
            }
        });
        if !relevant {
            continue;
        }
        let Some(source) = read_source(path) else {
            continue;
        };
        for (index, import) in file.imports.iter().enumerate() {
            let (start, end) = file.import_ranges.get(index).copied().unwrap_or((0, 0));
            if end <= start || end > source.len() {
                continue;
            }
            let raw = &source[start..end];
            let import = import.trim();
            let matches = if let Some(owner) = static_import_owner(import) {
                owner == fqn
            } else if import.ends_with(".*") {
                false
            } else {
                let path = import.strip_prefix("static ").unwrap_or(import).trim();
                path == fqn
            };
            if !matches {
                continue;
            }
            for (offset, length) in identifier_occurrences(raw, &old_name) {
                push_edit(
                    &mut files,
                    path,
                    start + offset,
                    start + offset + length,
                    new_name,
                );
            }
        }
    }

    let mut rename_file = None;
    let stem = location
        .path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default();
    let top_level = index.files.get(&location.path).is_some_and(|file| {
        file.symbols.iter().any(|symbol| {
            symbol.kind.is_type()
                && symbol.container.is_none()
                && symbol.name == old_name
                && crate::semantic::type_fqn(file, symbol) == fqn
        })
    });
    if top_level && stem == old_name {
        let target = location.path.with_file_name(format!("{new_name}.java"));
        if target != location.path {
            if target.exists() {
                return Err(format!("o arquivo {} já existe", target.display()));
            }
            rename_file = Some((location.path.clone(), target));
        }
    }

    files.retain(|file| !file.edits.is_empty());
    for file in &mut files {
        file.edits
            .sort_by(|left, right| right.start_byte.cmp(&left.start_byte));
        file.edits
            .dedup_by(|left, right| left.start_byte == right.start_byte);
    }
    if files.is_empty() {
        return Err("nenhuma ocorrência encontrada".to_owned());
    }
    Ok(RenamePlan {
        old_name,
        new_name: new_name.to_owned(),
        kind: RenameKind::Type,
        files,
        rename_file,
        warnings: rename_warnings(),
    })
}

/// Plan a method/field rename using semantic member references.
pub fn plan_member_rename(
    target: &MemberTarget,
    new_name: &str,
    project: &ProjectTypes,
    index: &ProjectIndex,
    classpath: &ClasspathIndex,
) -> Result<RenamePlan, String> {
    validate_identifier(new_name)?;
    let (path, container, type_fqn, name, is_call) = match target {
        MemberTarget::Project {
            path,
            container,
            type_fqn,
            name,
            is_call,
        } => (path, container, type_fqn, name, *is_call),
        MemberTarget::Classpath { .. } => {
            return Err("membros do classpath/JDK são somente leitura".to_owned())
        }
    };
    if name == new_name {
        return Err("o novo nome é igual ao atual".to_owned());
    }
    let owner = project
        .get(type_fqn)
        .map(|location| ResolvedType::Project(location.clone()))
        .ok_or_else(|| format!("tipo {type_fqn} não encontrado no índice"))?;
    if resolve_member_of_type(
        &owner,
        new_name,
        Some(is_call),
        project,
        Some(index),
        classpath,
    )
    .is_some()
    {
        return Err(format!(
            "já existe o membro '{new_name}' em {type_fqn} ou em um supertipo"
        ));
    }
    if is_call {
        guard_method_overrides(type_fqn, name, project, index, classpath)?;
    }

    let mut files: Vec<FileEdits> = Vec::new();
    let file = index
        .files
        .get(path)
        .ok_or_else(|| format!("arquivo {} não está no índice", path.display()))?;
    let mut declarations = 0;
    for symbol in &file.symbols {
        if symbol.name != *name || symbol.container.as_deref() != Some(container.as_str()) {
            continue;
        }
        let matches = if is_call {
            symbol.kind == SymbolKind::Method
        } else {
            matches!(symbol.kind, SymbolKind::Field | SymbolKind::EnumConstant)
        };
        if matches {
            declarations += 1;
            push_edit(
                &mut files,
                path,
                symbol.name_start_byte,
                symbol.name_end_byte,
                new_name,
            );
        }
    }
    if declarations == 0 {
        return Err("declaração do membro não encontrada".to_owned());
    }
    for reference in find_member_references(target, project, index, classpath) {
        push_edit(
            &mut files,
            &reference.file,
            reference.start_byte,
            reference.end_byte,
            new_name,
        );
    }
    for (other_path, other_file) in &index.files {
        let relevant = other_file.imports.iter().any(|import| {
            matches!(
                static_import_member(import),
                Some((owner, member)) if owner == type_fqn && member == name.as_str()
            )
        });
        if !relevant {
            continue;
        }
        let Some(source) = read_source(other_path) else {
            continue;
        };
        for (import_index, import) in other_file.imports.iter().enumerate() {
            let Some((owner, member)) = static_import_member(import) else {
                continue;
            };
            if owner != type_fqn || member != name.as_str() {
                continue;
            }
            let (start, end) = other_file
                .import_ranges
                .get(import_index)
                .copied()
                .unwrap_or((0, 0));
            if end <= start || end > source.len() {
                continue;
            }
            let raw = &source[start..end];
            for (offset, length) in identifier_occurrences(raw, name) {
                push_edit(
                    &mut files,
                    other_path,
                    start + offset,
                    start + offset + length,
                    new_name,
                );
            }
        }
    }

    files.retain(|file| !file.edits.is_empty());
    for file in &mut files {
        file.edits
            .sort_by(|left, right| right.start_byte.cmp(&left.start_byte));
        file.edits
            .dedup_by(|left, right| left.start_byte == right.start_byte);
    }
    Ok(RenamePlan {
        old_name: name.clone(),
        new_name: new_name.to_owned(),
        kind: if is_call {
            RenameKind::Method
        } else {
            RenameKind::Field
        },
        files,
        rename_file: None,
        warnings: rename_warnings(),
    })
}

/// Find the local binding under the cursor without planning a rename.
/// Returns `(name, declaration_byte)`; member names (`obj.field`) are rejected.
pub fn local_binding_at(source: &str, cursor: usize) -> Option<(String, usize)> {
    let byte = identifier_byte(source, cursor);
    let mut parser = Parser::new();
    let language: tree_sitter::Language = tree_sitter_java::LANGUAGE.into();
    if parser.set_language(&language).is_err() {
        return None;
    }
    let tree = parser.parse(source, None)?;
    let identifier = find_identifier(tree.root_node(), byte)?;
    if is_member_name(identifier) {
        return None;
    }
    let name = identifier.utf8_text(source.as_bytes()).ok()?.to_owned();
    let bindings = parse_local_bindings(source);
    let binding = bindings
        .iter()
        .filter(|binding| binding.name == name)
        .filter(|binding| {
            (binding.name_start_byte <= byte && byte < binding.name_end_byte)
                || (binding.declaration_start_byte <= byte
                    && binding.scope_start_byte <= byte
                    && byte < binding.scope_end_byte)
        })
        .min_by_key(|binding| {
            (
                binding
                    .scope_end_byte
                    .saturating_sub(binding.scope_start_byte),
                std::cmp::Reverse(binding.declaration_start_byte),
            )
        })?;
    Some((name, binding.declaration_start_byte))
}

/// Plan a local variable/parameter rename in a single buffer.
pub fn plan_local_rename(
    source: &str,
    cursor: usize,
    new_name: &str,
) -> Result<LocalRenamePlan, String> {
    validate_identifier(new_name)?;
    let byte = identifier_byte(source, cursor);
    let mut parser = Parser::new();
    let language: tree_sitter::Language = tree_sitter_java::LANGUAGE.into();
    parser
        .set_language(&language)
        .map_err(|error| format!("falha ao carregar a gramática Java: {error}"))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "não foi possível analisar o arquivo".to_owned())?;
    let root = tree.root_node();
    let identifier = find_identifier(root, byte)
        .ok_or_else(|| "coloque o cursor sobre uma variável local ou parâmetro".to_owned())?;
    if is_member_name(identifier) {
        return Err("o cursor está sobre um membro, não sobre uma variável local".to_owned());
    }
    let old_name = identifier
        .utf8_text(source.as_bytes())
        .unwrap_or("")
        .to_owned();
    if old_name.is_empty() {
        return Err("identificador inválido".to_owned());
    }
    if old_name == new_name {
        return Err("o novo nome é igual ao atual".to_owned());
    }

    let bindings = parse_local_bindings(source);
    let target = bindings
        .iter()
        .filter(|binding| binding.name == old_name)
        .filter(|binding| {
            (binding.name_start_byte <= byte && byte < binding.name_end_byte)
                || (binding.declaration_start_byte <= byte
                    && binding.scope_start_byte <= byte
                    && byte < binding.scope_end_byte)
        })
        .min_by_key(|binding| {
            (
                binding
                    .scope_end_byte
                    .saturating_sub(binding.scope_start_byte),
                std::cmp::Reverse(binding.declaration_start_byte),
            )
        })
        .ok_or_else(|| "o identificador não é uma variável local/parâmetro".to_owned())?;

    if bindings.iter().any(|other| {
        other.name == old_name
            && other.name_start_byte != target.name_start_byte
            && other.scope_start_byte >= target.scope_start_byte
            && other.scope_end_byte <= target.scope_end_byte
    }) {
        return Err(format!(
            "a variável '{old_name}' é sombreada em um escopo interno; rename cancelado por segurança"
        ));
    }

    let mut edits = Vec::new();
    collect_local_occurrences(
        root,
        source.as_bytes(),
        target,
        &bindings,
        &mut edits,
        new_name,
    );
    if edits.is_empty() {
        return Err("nenhuma ocorrência encontrada".to_owned());
    }
    edits.sort_by(|left, right| right.start_byte.cmp(&left.start_byte));
    edits.dedup_by(|left, right| left.start_byte == right.start_byte);
    Ok(LocalRenamePlan {
        old_name,
        new_name: new_name.to_owned(),
        edits,
    })
}

/// Apply byte-range edits to source text. Edits may arrive in any order and
/// must not overlap.
pub fn apply_text_edits(source: &str, edits: &[TextEdit]) -> Result<String, String> {
    let mut edits = edits.to_vec();
    edits.sort_by(|left, right| right.start_byte.cmp(&left.start_byte));
    let mut result = source.to_owned();
    let mut last_start = source.len() + 1;
    for edit in edits {
        if edit.end_byte > source.len() || edit.start_byte > edit.end_byte {
            return Err("edição fora dos limites do arquivo".to_owned());
        }
        if edit.end_byte > last_start {
            return Err("edições sobrepostas".to_owned());
        }
        result.replace_range(edit.start_byte..edit.end_byte, &edit.replacement);
        last_start = edit.start_byte;
    }
    Ok(result)
}

pub fn validate_identifier(name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("informe um nome".to_owned());
    }
    if name != name.trim() || name.contains(char::is_whitespace) {
        return Err("o nome não pode conter espaços".to_owned());
    }
    let mut characters = name.chars();
    let Some(first) = characters.next() else {
        return Err("informe um nome".to_owned());
    };
    if !(first.is_alphabetic() || first == '_' || first == '$') {
        return Err("o nome deve começar com letra, '_' ou '$'".to_owned());
    }
    if !characters
        .all(|character| character.is_alphanumeric() || character == '_' || character == '$')
    {
        return Err("o nome contém caracteres inválidos".to_owned());
    }
    if is_java_keyword(name) {
        return Err(format!("'{name}' é palavra reservada do Java"));
    }
    Ok(())
}

fn is_java_keyword(name: &str) -> bool {
    matches!(
        name,
        "abstract"
            | "assert"
            | "boolean"
            | "break"
            | "byte"
            | "case"
            | "catch"
            | "char"
            | "class"
            | "const"
            | "continue"
            | "default"
            | "do"
            | "double"
            | "else"
            | "enum"
            | "extends"
            | "final"
            | "finally"
            | "float"
            | "for"
            | "goto"
            | "if"
            | "implements"
            | "import"
            | "instanceof"
            | "int"
            | "interface"
            | "long"
            | "native"
            | "new"
            | "package"
            | "private"
            | "protected"
            | "public"
            | "return"
            | "short"
            | "static"
            | "strictfp"
            | "super"
            | "switch"
            | "synchronized"
            | "this"
            | "throw"
            | "throws"
            | "transient"
            | "try"
            | "void"
            | "volatile"
            | "while"
            | "true"
            | "false"
            | "null"
            | "var"
            | "_"
            | "record"
            | "sealed"
            | "permits"
            | "yield"
    )
}

fn guard_method_overrides(
    type_fqn: &str,
    name: &str,
    project: &ProjectTypes,
    index: &ProjectIndex,
    classpath: &ClasspathIndex,
) -> Result<(), String> {
    // Renaming an override would desynchronize the supertype declaration.
    for super_type in all_supertypes(type_fqn, project, index, classpath) {
        if resolve_member_of_type(
            &super_type,
            name,
            Some(true),
            project,
            Some(index),
            classpath,
        )
        .is_some()
        {
            return Err(format!(
                "'{name}' sobrescreve/implementa um método de {}; rename cancelado por segurança",
                super_type.name()
            ));
        }
    }
    // Renaming an interface/base method would leave implementations behind.
    for (other_fqn, location) in &project.by_fqn {
        if other_fqn == type_fqn {
            continue;
        }
        let supers = all_supertypes(other_fqn, project, index, classpath);
        if !supers
            .iter()
            .any(|super_type| super_type.name() == type_fqn)
        {
            continue;
        }
        let Some(file) = index.files.get(&location.path) else {
            continue;
        };
        let package = file.package.as_deref().unwrap_or("");
        let container = location
            .fqn
            .strip_prefix(package)
            .unwrap_or(&location.fqn)
            .trim_start_matches('.');
        if file.symbols.iter().any(|symbol| {
            symbol.kind == SymbolKind::Method
                && symbol.name == name
                && symbol.container.as_deref() == Some(container)
        }) {
            return Err(format!(
                "'{name}' é sobrescrito/implementado por {other_fqn}; rename cancelado por segurança"
            ));
        }
    }
    Ok(())
}

fn all_supertypes(
    fqn: &str,
    project: &ProjectTypes,
    index: &ProjectIndex,
    classpath: &ClasspathIndex,
) -> Vec<ResolvedType> {
    let mut found = Vec::new();
    let mut visited = std::collections::HashSet::new();
    let mut pending = supertypes_of(fqn, project, index, classpath);
    while let Some(super_type) = pending.pop() {
        if !visited.insert(super_type.name().to_owned()) {
            continue;
        }
        if let ResolvedType::Project(location) = &super_type {
            pending.extend(supertypes_of(&location.fqn, project, index, classpath));
        }
        found.push(super_type);
    }
    found
}

fn collect_local_occurrences(
    node: Node<'_>,
    source: &[u8],
    target: &LocalBinding,
    bindings: &[LocalBinding],
    edits: &mut Vec<TextEdit>,
    new_name: &str,
) {
    if node.kind() == "identifier" {
        let start = node.start_byte();
        let end = node.end_byte();
        if start >= target.declaration_start_byte
            && start >= target.scope_start_byte
            && end <= target.scope_end_byte
            && node.utf8_text(source).unwrap_or("") == target.name
            && !is_member_name(node)
            && (!is_declaration_name(node)
                || (start == target.name_start_byte && end == target.name_end_byte))
            && !bindings.iter().any(|binding| {
                binding.name == target.name
                    && binding.name_start_byte != target.name_start_byte
                    && start >= binding.scope_start_byte
                    && end <= binding.scope_end_byte
            })
        {
            edits.push(TextEdit {
                start_byte: start,
                end_byte: end,
                replacement: new_name.to_owned(),
            });
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_local_occurrences(child, source, target, bindings, edits, new_name);
    }
}

fn is_member_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "method_invocation" => parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id()),
        "field_access" => parent
            .child_by_field_name("field")
            .is_some_and(|field| field.id() == node.id()),
        "method_reference" => {
            let mut cursor = parent.walk();
            let last = parent.named_children(&mut cursor).last();
            last.is_some_and(|last| last.id() == node.id())
        }
        _ => false,
    }
}

fn is_declaration_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if matches!(
        parent.kind(),
        "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration"
            | "method_declaration"
            | "constructor_declaration"
            | "compact_constructor_declaration"
            | "enum_constant"
            | "labeled_statement"
            | "break_statement"
            | "continue_statement"
            | "marker_annotation"
            | "annotation"
    ) {
        return true;
    }
    parent
        .child_by_field_name("name")
        .is_some_and(|name| name.id() == node.id())
        && matches!(
            parent.kind(),
            "variable_declarator"
                | "formal_parameter"
                | "catch_formal_parameter"
                | "enhanced_for_statement"
                | "resource"
        )
}

fn find_identifier(node: Node<'_>, byte: usize) -> Option<Node<'_>> {
    if node.start_byte() > byte || node.end_byte() <= byte {
        return None;
    }
    if node.kind() == "identifier" {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(found) = find_identifier(child, byte) {
            return Some(found);
        }
    }
    None
}

fn push_edit(
    files: &mut Vec<FileEdits>,
    path: &Path,
    start_byte: usize,
    end_byte: usize,
    replacement: &str,
) {
    let edit = TextEdit {
        start_byte,
        end_byte,
        replacement: replacement.to_owned(),
    };
    if let Some(file) = files.iter_mut().find(|file| file.path == path) {
        if !file
            .edits
            .iter()
            .any(|known| known.start_byte == edit.start_byte && known.end_byte == edit.end_byte)
        {
            file.edits.push(edit);
        }
    } else {
        files.push(FileEdits {
            path: path.to_path_buf(),
            edits: vec![edit],
        });
    }
}

fn static_import_owner(import: &str) -> Option<&str> {
    let path = import.trim().strip_prefix("static ")?;
    let (owner, _member) = path.rsplit_once('.')?;
    Some(owner)
}

/// Split `import static owner.member;` into `(owner, member)`. Wildcards and
/// non-static imports return `None`.
fn static_import_member(import: &str) -> Option<(&str, &str)> {
    let path = import.trim().strip_prefix("static ")?.trim_end_matches(';');
    let (owner, member) = path.rsplit_once('.')?;
    if member == "*" || owner.is_empty() || member.is_empty() {
        return None;
    }
    Some((owner.trim(), member.trim()))
}

fn identifier_occurrences(haystack: &str, needle: &str) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut start = 0;
    while let Some(position) = haystack[start..].find(needle) {
        let absolute = start + position;
        let end = absolute + needle.len();
        let left_ok = haystack[..absolute]
            .chars()
            .next_back()
            .is_none_or(|character| !is_identifier_char(character));
        let right_ok = haystack[end..]
            .chars()
            .next()
            .is_none_or(|character| !is_identifier_char(character));
        if left_ok && right_ok {
            found.push((absolute, needle.len()));
        }
        start = end;
    }
    found
}

fn read_source(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    String::from_utf8(bytes).ok()
}

fn identifier_byte(source: &str, cursor: usize) -> usize {
    let characters: Vec<char> = source.chars().collect();
    if characters.is_empty() {
        return 0;
    }
    let index = cursor.min(characters.len() - 1);
    if is_identifier_char(characters[index]) {
        return byte_index_of_char(source, index);
    }
    if index > 0 && is_identifier_char(characters[index - 1]) {
        return byte_index_of_char(source, index - 1);
    }
    byte_index_of_char(source, index)
}

fn byte_index_of_char(source: &str, char_index: usize) -> usize {
    source
        .char_indices()
        .nth(char_index)
        .map(|(byte, _)| byte)
        .unwrap_or(source.len())
}

fn is_identifier_char(character: char) -> bool {
    character.is_alphanumeric() || character == '_' || character == '$'
}

fn rename_warnings() -> Vec<String> {
    vec![
        "Javadoc, strings e referências por reflexão não são atualizados.".to_owned(),
        "Rename de workspace exige buffers Java salvos (buffers sujos bloqueiam).".to_owned(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_source;

    #[test]
    fn validates_java_identifiers() {
        assert!(validate_identifier("user").is_ok());
        assert!(validate_identifier("_User2").is_ok());
        assert!(validate_identifier("").is_err());
        assert!(validate_identifier("2user").is_err());
        assert!(validate_identifier("class").is_err());
        assert!(validate_identifier("a b").is_err());
    }

    #[test]
    fn local_rename_keeps_scope_and_ignores_other_locals() {
        let source = r#"
class App {
    void run() {
        int count = 0;
        for (int i = 0; i < 2; i++) {
            count += i;
        }
        count++;
    }
}
"#;
        let cursor = source.find("count").unwrap() + 2;
        let plan = plan_local_rename(source, cursor, "total").unwrap();
        assert_eq!(plan.old_name, "count");
        let updated = apply_text_edits(source, &plan.edits).unwrap();
        assert!(updated.contains("int total = 0"));
        assert_eq!(updated.matches("total").count(), 3);
        assert!(!updated.contains("count"));
    }

    #[test]
    fn local_rename_aborts_on_shadowing() {
        let source = r#"
class App {
    void run() {
        int count = 0;
        { int count = 1; count++; }
        count++;
    }
}
"#;
        let cursor = source.find("count = 0").unwrap() + 2;
        let error = plan_local_rename(source, cursor, "total").unwrap_err();
        assert!(error.contains("sombreada"));
    }

    #[test]
    fn type_rename_updates_declarations_and_references() {
        let user_path = PathBuf::from("com/acme/User.java");
        let app_path = PathBuf::from("com/acme/App.java");
        let user = parse_source(&user_path, "package com.acme; public class User {}");
        let app = parse_source(&app_path, "package com.acme; class App { User user; }");
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
        let plan = plan_type_rename(
            "com.acme.User",
            "Account",
            &types,
            &index,
            &ClasspathIndex::default(),
        )
        .unwrap();
        assert_eq!(plan.total_edits(), 2);
        assert_eq!(
            plan.rename_file.as_ref().map(|(_, to)| to.clone()),
            Some(user_path.with_file_name("Account.java"))
        );
    }

    #[test]
    fn local_rename_accepts_cursor_after_identifier_and_rejects_members() {
        let source = "class App { String value; void run() { String value = \"x\"; System.out.println(this.value.length()); System.out.println(value); } }";
        let cursor = source.rfind("value").unwrap() + "value".len();
        let plan = plan_local_rename(source, cursor, "text").unwrap();
        assert_eq!(plan.edits.len(), 2);
        let member = source.find("this.value").unwrap() + "this.".len() + 1;
        let error = plan_local_rename(source, member, "text").unwrap_err();
        assert!(error.contains("membro"));
    }

    #[test]
    fn local_rename_updates_parameters() {
        let source = "class App { void run(String name) { System.out.println(name); } }";
        let cursor = source.find("name").unwrap() + 1;
        let plan = plan_local_rename(source, cursor, "value").unwrap();
        let updated = apply_text_edits(source, &plan.edits).unwrap();
        assert!(updated.contains("String value"));
        assert!(updated.contains("println(value)"));
    }

    #[test]
    fn type_rename_updates_imports() {
        let dir = tempfile::tempdir().unwrap();
        let user_path = dir.path().join("com/acme/User.java");
        let app_path = dir.path().join("com/other/App.java");
        std::fs::create_dir_all(user_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(app_path.parent().unwrap()).unwrap();
        let user_source = "package com.acme; public class User {}";
        let app_source = "package com.other; import com.acme.User; class App { User user; }";
        std::fs::write(&user_path, user_source).unwrap();
        std::fs::write(&app_path, app_source).unwrap();
        let user = parse_source(&user_path, user_source);
        let app = parse_source(&app_path, app_source);
        let project = reclipse_core::Project {
            root: dir.path().to_path_buf(),
            name: "test".to_owned(),
            build_system: reclipse_core::BuildSystem::Plain,
            source_roots: Vec::new(),
            java_files: vec![user_path.clone(), app_path.clone()],
            all_files: vec![user_path.clone(), app_path.clone()],
        };
        let index = ProjectIndex::from_files(project, vec![user, app], 0);
        let types = ProjectTypes::build(&index);
        let plan = plan_type_rename(
            "com.acme.User",
            "Account",
            &types,
            &index,
            &ClasspathIndex::default(),
        )
        .unwrap();
        let app_edits = plan
            .files
            .iter()
            .find(|file| file.path == app_path)
            .unwrap();
        assert!(app_edits.len() >= 2);
        let updated = apply_text_edits(app_source, &app_edits.edits).unwrap();
        assert!(updated.contains("import com.acme.Account;"));
        assert!(updated.contains("Account user;"));
    }

    #[test]
    fn type_rename_plan_produces_a_consistent_project() {
        let dir = tempfile::tempdir().unwrap();
        let user_path = dir.path().join("com/acme/User.java");
        let app_path = dir.path().join("com/acme/App.java");
        std::fs::create_dir_all(user_path.parent().unwrap()).unwrap();
        let user_source =
            "package com.acme; public class User { public String name; public User(String name) { this.name = name; } }";
        let app_source =
            "package com.acme; class App { User user; User make() { return new User(\"x\"); } }";
        std::fs::write(&user_path, user_source).unwrap();
        std::fs::write(&app_path, app_source).unwrap();
        let user = parse_source(&user_path, user_source);
        let app = parse_source(&app_path, app_source);
        let project = reclipse_core::Project {
            root: dir.path().to_path_buf(),
            name: "test".to_owned(),
            build_system: reclipse_core::BuildSystem::Plain,
            source_roots: Vec::new(),
            java_files: vec![user_path.clone(), app_path.clone()],
            all_files: vec![user_path.clone(), app_path.clone()],
        };
        let index = ProjectIndex::from_files(project, vec![user, app], 0);
        let types = ProjectTypes::build(&index);
        let plan = plan_type_rename(
            "com.acme.User",
            "Account",
            &types,
            &index,
            &ClasspathIndex::default(),
        )
        .unwrap();
        for file in &plan.files {
            let text = std::fs::read_to_string(&file.path).unwrap();
            let updated = apply_text_edits(&text, &file.edits).unwrap();
            std::fs::write(&file.path, updated).unwrap();
        }
        let (from, to) = plan.rename_file.clone().unwrap();
        std::fs::rename(&from, &to).unwrap();

        let project = reclipse_core::discover_project(dir.path()).unwrap();
        let index = crate::build_index(project, None);
        let types = ProjectTypes::build(&index);
        assert!(types.get("com.acme.Account").is_some());
        assert!(types.get("com.acme.User").is_none());
        let app_source = std::fs::read_to_string(&app_path).unwrap();
        assert!(!app_source.contains("User"));
        assert!(app_source.contains("Account make()"));
        assert!(app_source.contains("new Account(\"x\")"));
        let user_source = std::fs::read_to_string(&to).unwrap();
        assert!(user_source.contains("public Account(String name)"));
        assert!(!user_source.contains("User"));
    }

    #[test]
    fn member_rename_updates_overloads_and_references() {
        let dir = tempfile::tempdir().unwrap();
        let user_path = dir.path().join("com/acme/User.java");
        let app_path = dir.path().join("com/acme/App.java");
        std::fs::create_dir_all(user_path.parent().unwrap()).unwrap();
        let user_source = "package com.acme; public class User { public void save() {} public void save(int times) {} }";
        let app_source =
            "package com.acme; class App { void run(User user) { user.save(); user.save(2); } }";
        std::fs::write(&user_path, user_source).unwrap();
        std::fs::write(&app_path, app_source).unwrap();
        let user = parse_source(&user_path, user_source);
        let app = parse_source(&app_path, app_source);
        let project = reclipse_core::Project {
            root: dir.path().to_path_buf(),
            name: "test".to_owned(),
            build_system: reclipse_core::BuildSystem::Plain,
            source_roots: Vec::new(),
            java_files: vec![user_path.clone(), app_path.clone()],
            all_files: vec![user_path.clone(), app_path.clone()],
        };
        let index = ProjectIndex::from_files(project, vec![user, app], 0);
        let types = ProjectTypes::build(&index);
        let target = MemberTarget::Project {
            path: user_path.clone(),
            container: "User".to_owned(),
            type_fqn: "com.acme.User".to_owned(),
            name: "save".to_owned(),
            is_call: true,
        };
        let plan = plan_member_rename(
            &target,
            "persist",
            &types,
            &index,
            &ClasspathIndex::default(),
        )
        .unwrap();
        assert_eq!(plan.total_edits(), 4);
        let updated_user = apply_text_edits(
            user_source,
            &plan
                .files
                .iter()
                .find(|file| file.path == user_path)
                .unwrap()
                .edits,
        )
        .unwrap();
        assert_eq!(updated_user.matches("persist").count(), 2);
        let updated_app = apply_text_edits(
            app_source,
            &plan
                .files
                .iter()
                .find(|file| file.path == app_path)
                .unwrap()
                .edits,
        )
        .unwrap();
        assert_eq!(updated_app.matches("persist").count(), 2);
    }

    #[test]
    fn member_rename_rejects_overrides_and_implementations() {
        let dir = tempfile::tempdir().unwrap();
        let base_path = dir.path().join("Base.java");
        let sub_path = dir.path().join("Sub.java");
        let base_source = "package com.acme; public class Base { public void save() {} }";
        let sub_source =
            "package com.acme; public class Sub extends Base { @Override public void save() {} }";
        std::fs::write(&base_path, base_source).unwrap();
        std::fs::write(&sub_path, sub_source).unwrap();
        let base = parse_source(&base_path, base_source);
        let sub = parse_source(&sub_path, sub_source);
        let project = reclipse_core::Project {
            root: dir.path().to_path_buf(),
            name: "test".to_owned(),
            build_system: reclipse_core::BuildSystem::Plain,
            source_roots: Vec::new(),
            java_files: vec![base_path.clone(), sub_path.clone()],
            all_files: vec![base_path.clone(), sub_path.clone()],
        };
        let index = ProjectIndex::from_files(project, vec![base, sub], 0);
        let types = ProjectTypes::build(&index);
        let target = MemberTarget::Project {
            path: base_path,
            container: "Base".to_owned(),
            type_fqn: "com.acme.Base".to_owned(),
            name: "save".to_owned(),
            is_call: true,
        };
        let error = plan_member_rename(
            &target,
            "persist",
            &types,
            &index,
            &ClasspathIndex::default(),
        )
        .unwrap_err();
        assert!(error.contains("sobrescrito"));
    }

    #[test]
    fn member_rename_updates_static_imports() {
        let dir = tempfile::tempdir().unwrap();
        let user_path = dir.path().join("com/acme/User.java");
        let app_path = dir.path().join("com/acme/App.java");
        std::fs::create_dir_all(user_path.parent().unwrap()).unwrap();
        let user_source = "package com.acme; public class User { public static void save() {} }";
        let app_source =
            "package com.acme; import static com.acme.User.save; class App { void run() { save(); } }";
        std::fs::write(&user_path, user_source).unwrap();
        std::fs::write(&app_path, app_source).unwrap();
        let user = parse_source(&user_path, user_source);
        let app = parse_source(&app_path, app_source);
        let project = reclipse_core::Project {
            root: dir.path().to_path_buf(),
            name: "test".to_owned(),
            build_system: reclipse_core::BuildSystem::Plain,
            source_roots: Vec::new(),
            java_files: vec![user_path.clone(), app_path.clone()],
            all_files: vec![user_path.clone(), app_path.clone()],
        };
        let index = ProjectIndex::from_files(project, vec![user, app], 0);
        let types = ProjectTypes::build(&index);
        let target = MemberTarget::Project {
            path: user_path.clone(),
            container: "User".to_owned(),
            type_fqn: "com.acme.User".to_owned(),
            name: "save".to_owned(),
            is_call: true,
        };
        let plan = plan_member_rename(
            &target,
            "persist",
            &types,
            &index,
            &ClasspathIndex::default(),
        )
        .unwrap();
        let app_edits = plan
            .files
            .iter()
            .find(|file| file.path == app_path)
            .unwrap();
        let updated = apply_text_edits(app_source, &app_edits.edits).unwrap();
        assert!(updated.contains("import static com.acme.User.persist;"));
        assert!(updated.contains("persist();"));
    }

    #[test]
    fn member_rename_finds_static_wildcard_references() {
        let dir = tempfile::tempdir().unwrap();
        let user_path = dir.path().join("com/acme/User.java");
        let app_path = dir.path().join("com/acme/App.java");
        std::fs::create_dir_all(user_path.parent().unwrap()).unwrap();
        let user_source = "package com.acme; public class User { public static void save() {} }";
        let app_source =
            "package com.acme; import static com.acme.User.*; class App { void run() { save(); } }";
        std::fs::write(&user_path, user_source).unwrap();
        std::fs::write(&app_path, app_source).unwrap();
        let user = parse_source(&user_path, user_source);
        let app = parse_source(&app_path, app_source);
        let project = reclipse_core::Project {
            root: dir.path().to_path_buf(),
            name: "test".to_owned(),
            build_system: reclipse_core::BuildSystem::Plain,
            source_roots: Vec::new(),
            java_files: vec![user_path, app_path.clone()],
            all_files: vec![app_path.clone()],
        };
        let index = ProjectIndex::from_files(project, vec![user, app], 0);
        let types = ProjectTypes::build(&index);
        let target = MemberTarget::Project {
            path: PathBuf::from("com/acme/User.java"),
            container: "User".to_owned(),
            type_fqn: "com.acme.User".to_owned(),
            name: "save".to_owned(),
            is_call: true,
        };
        let references =
            find_member_references(&target, &types, &index, &ClasspathIndex::default());
        assert_eq!(references.len(), 1);
        assert_eq!(references[0].file, app_path);
    }

    #[test]
    fn member_rename_rejects_collisions() {
        let dir = tempfile::tempdir().unwrap();
        let user_path = dir.path().join("User.java");
        let user_source =
            "package com.acme; public class User { void save() {} void persist() {} }";
        std::fs::write(&user_path, user_source).unwrap();
        let user = parse_source(&user_path, user_source);
        let project = reclipse_core::Project {
            root: dir.path().to_path_buf(),
            name: "test".to_owned(),
            build_system: reclipse_core::BuildSystem::Plain,
            source_roots: Vec::new(),
            java_files: vec![user_path.clone()],
            all_files: vec![user_path.clone()],
        };
        let index = ProjectIndex::from_files(project, vec![user], 0);
        let types = ProjectTypes::build(&index);
        let target = MemberTarget::Project {
            path: user_path,
            container: "User".to_owned(),
            type_fqn: "com.acme.User".to_owned(),
            name: "save".to_owned(),
            is_call: true,
        };
        assert!(plan_member_rename(
            &target,
            "persist",
            &types,
            &index,
            &ClasspathIndex::default()
        )
        .is_err());
    }

    #[test]
    fn edits_are_applied_bottom_up() {
        let source = "aaa bbb aaa";
        let edits = vec![
            TextEdit {
                start_byte: 0,
                end_byte: 3,
                replacement: "x".to_owned(),
            },
            TextEdit {
                start_byte: 8,
                end_byte: 11,
                replacement: "x".to_owned(),
            },
        ];
        assert_eq!(apply_text_edits(source, &edits).unwrap(), "x bbb x");
    }

    #[test]
    fn missing_symbol_prevents_rename() {
        let file = parse_source(Path::new("A.java"), "class A {}");
        let project = reclipse_core::Project {
            root: PathBuf::from("."),
            name: "test".to_owned(),
            build_system: reclipse_core::BuildSystem::Plain,
            source_roots: Vec::new(),
            java_files: vec![file.path.clone()],
            all_files: vec![file.path.clone()],
        };
        let index = ProjectIndex::from_files(project, vec![file], 0);
        let types = ProjectTypes::build(&index);
        assert!(plan_type_rename(
            "missing.Type",
            "Other",
            &types,
            &index,
            &ClasspathIndex::default()
        )
        .is_err());
    }
}
