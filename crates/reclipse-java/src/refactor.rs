//! Conservative rename planning over the syntax/semantic index.
//!
//! Rename never guesses: unresolved or ambiguous references are left untouched,
//! shadowed locals abort the operation, and classpath members are read-only.
//! Javadoc links and module-info directives are rewritten for type/package
//! renames; strings, resources and reflection are not.

use std::fs;
use std::path::{Path, PathBuf};

use reclipse_classpath::ClasspathIndex;
use reclipse_core::Project;
use tree_sitter::{Node, Parser};

use crate::semantic::{
    find_member_references, find_type_references, resolve_member_of_type, supertypes_of,
    MemberTarget, ProjectTypes, ResolvedType,
};
use crate::{parse_local_bindings, FileSymbols, LocalBinding, ProjectIndex, SymbolKind};

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
    MoveType,
    Method,
    Field,
    Package,
    Local,
}

impl RenameKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Type => "tipo",
            Self::MoveType => "tipo (mover)",
            Self::Method => "método",
            Self::Field => "campo",
            Self::Package => "pacote",
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
    /// File moves (package rename, type rename). Paths are old -> new.
    pub moves: Vec<(PathBuf, PathBuf)>,
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

/// Opt-in textual occurrences of `old` inside Java string/char literals and
/// comments (WTP-style "update textual matches"). Whole-word only; the
/// semantic rename stays authoritative, these edits are additive.
pub fn plan_textual_java_edits(source: &str, old: &str, new: &str) -> Vec<TextEdit> {
    if old.is_empty() || old == new {
        return Vec::new();
    }
    let chars: Vec<char> = source.chars().collect();
    // Byte offsets per char index.
    let mut byte_offsets = Vec::with_capacity(chars.len() + 1);
    let mut byte = 0;
    for character in &chars {
        byte_offsets.push(byte);
        byte += character.len_utf8();
    }
    byte_offsets.push(byte);
    let mut edits = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let character = chars[index];
        if character == '/' && chars.get(index + 1) == Some(&'/') {
            index = scan_textual_line(&chars, &byte_offsets, index + 2, old, new, &mut edits);
        } else if character == '/' && chars.get(index + 1) == Some(&'*') {
            index = scan_textual_block(&chars, &byte_offsets, index + 2, old, new, &mut edits);
        } else if character == '"' || character == '\'' {
            index = scan_textual_string(&chars, &byte_offsets, index, old, new, &mut edits);
        } else {
            index += 1;
        }
    }
    edits
}

fn scan_textual_line(
    chars: &[char],
    bytes: &[usize],
    mut index: usize,
    old: &str,
    new: &str,
    edits: &mut Vec<TextEdit>,
) -> usize {
    let start = index;
    while index < chars.len() && chars[index] != '\n' {
        index += 1;
    }
    collect_word_edits(chars, bytes, start, index, old, new, edits);
    index
}

fn scan_textual_block(
    chars: &[char],
    bytes: &[usize],
    mut index: usize,
    old: &str,
    new: &str,
    edits: &mut Vec<TextEdit>,
) -> usize {
    let start = index;
    while index < chars.len() {
        if chars[index] == '*' && chars.get(index + 1) == Some(&'/') {
            collect_word_edits(chars, bytes, start, index, old, new, edits);
            return index + 2;
        }
        index += 1;
    }
    collect_word_edits(chars, bytes, start, index.min(chars.len()), old, new, edits);
    index
}

fn scan_textual_string(
    chars: &[char],
    bytes: &[usize],
    index: usize,
    old: &str,
    new: &str,
    edits: &mut Vec<TextEdit>,
) -> usize {
    let quote = chars[index];
    let mut cursor = index + 1;
    // Text blocks (`"""`) may span newlines; other literals end at EOL.
    if quote == '"' && chars.get(cursor) == Some(&'"') && chars.get(cursor + 1) == Some(&'"') {
        cursor += 2;
        let start = cursor;
        while cursor < chars.len() {
            if chars[cursor] == '"'
                && chars.get(cursor + 1) == Some(&'"')
                && chars.get(cursor + 2) == Some(&'"')
            {
                collect_word_edits(chars, bytes, start, cursor, old, new, edits);
                return cursor + 3;
            }
            cursor += 1;
        }
        collect_word_edits(
            chars,
            bytes,
            start,
            cursor.min(chars.len()),
            old,
            new,
            edits,
        );
        return cursor;
    }
    let start = cursor;
    while cursor < chars.len() && chars[cursor] != '\n' {
        if chars[cursor] == '\\' {
            cursor += 2;
            continue;
        }
        if chars[cursor] == quote {
            collect_word_edits(chars, bytes, start, cursor, old, new, edits);
            return cursor + 1;
        }
        cursor += 1;
    }
    // Unterminated literal: still offer matches up to EOL.
    collect_word_edits(
        chars,
        bytes,
        start,
        cursor.min(chars.len()),
        old,
        new,
        edits,
    );
    cursor
}

fn collect_word_edits(
    chars: &[char],
    bytes: &[usize],
    start: usize,
    end: usize,
    old: &str,
    new: &str,
    edits: &mut Vec<TextEdit>,
) {
    let end = end.min(chars.len());
    if start >= end || old.is_empty() {
        return;
    }
    let text: String = chars[start..end].iter().collect();
    let mut search = 0;
    while let Some(found) = text[search..].find(old) {
        let absolute = start + search + found;
        let before = absolute.checked_sub(1).and_then(|index| chars.get(index));
        let after = chars.get(absolute + old.chars().count());
        if is_word_boundary(before) && is_word_boundary(after) {
            edits.push(TextEdit {
                start_byte: bytes[absolute],
                end_byte: bytes[absolute + old.chars().count()],
                replacement: new.to_owned(),
            });
        }
        search += found + old.len().max(1);
    }
}

fn is_word_boundary(character: Option<&char>) -> bool {
    !matches!(character, Some(character) if character.is_alphanumeric() || *character == '_' || *character == '$')
}

/// Whole-word textual edits for resource files (`.properties`, ...).
/// Comment lines (`#`/`!`) are included: keys and values may reference names.
pub fn plan_textual_resource_edits(text: &str, old: &str, new: &str) -> Vec<TextEdit> {
    if old.is_empty() || old == new {
        return Vec::new();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut byte_offsets = Vec::with_capacity(chars.len() + 1);
    let mut byte = 0;
    for character in &chars {
        byte_offsets.push(byte);
        byte += character.len_utf8();
    }
    byte_offsets.push(byte);
    let mut edits = Vec::new();
    collect_word_edits(&chars, &byte_offsets, 0, chars.len(), old, new, &mut edits);
    edits
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
    if let Some(path) = index.files.iter().find_map(|(path, file)| {
        file.local_type_names
            .iter()
            .any(|name| name == &old_name)
            .then_some(path)
    }) {
        // Renaming the local itself is fine; renaming anything else while a
        // same-named local exists elsewhere is refused for safety.
        if path != &location.path {
            return Err(format!(
                "há um tipo local chamado '{old_name}' em {}; rename cancelado por segurança",
                path.display()
            ));
        }
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
    for path in index.files.keys() {
        let Some(source) = read_source(path) else {
            continue;
        };
        if path
            .file_name()
            .is_some_and(|name| name == "module-info.java")
        {
            for (start, length) in module_info_type_occurrences(&source, &old_name, fqn) {
                push_edit(&mut files, path, start, start + length, new_name);
            }
        }
        for (start, length) in javadoc_text_occurrences(&source, &old_name) {
            push_edit(&mut files, path, start, start + length, new_name);
        }
    }

    let mut moves = Vec::new();
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
            moves.push((location.path.clone(), target));
        }
    }

    files.retain(|file| !file.edits.is_empty());
    for file in &mut files {
        file.edits
            .sort_by_key(|edit| std::cmp::Reverse(edit.start_byte));
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
        moves,
        warnings: rename_warnings(),
    })
}

/// Plan moving a top-level type to another package. Updates the package
/// declaration, imports, fully-qualified references, Javadoc links and
/// module-info directives; same-package/import-less files receive an explicit
/// import.
pub fn plan_type_move(
    fqn: &str,
    new_package: &str,
    project: &Project,
    project_types: &ProjectTypes,
    index: &ProjectIndex,
    classpath: &ClasspathIndex,
) -> Result<RenamePlan, String> {
    let new_package = new_package.trim();
    if new_package.is_empty() {
        return Err("informe o pacote de destino".to_owned());
    }
    for segment in new_package.split('.') {
        validate_identifier(segment)?;
    }
    if new_package == "java"
        || new_package.starts_with("java.")
        || new_package.starts_with("javax.")
    {
        return Err("não é permitido mover para o namespace java/javax".to_owned());
    }
    let location = project_types
        .get(fqn)
        .ok_or_else(|| "o tipo não pertence ao projeto (classpath é somente leitura)".to_owned())?;
    let Some((old_package, simple_name)) = fqn.rsplit_once('.') else {
        return Err("mover tipos do pacote default não é suportado".to_owned());
    };
    if old_package == new_package {
        return Err("o tipo já está nesse pacote".to_owned());
    }
    let new_fqn = format!("{new_package}.{simple_name}");
    if project_types.get(&new_fqn).is_some() {
        return Err(format!("já existe o tipo {new_fqn} no projeto"));
    }
    if classpath.get_loose(&new_fqn).is_some() {
        return Err(format!("já existe o tipo {new_fqn} no classpath"));
    }
    let Some(file) = index.files.get(&location.path) else {
        return Err("arquivo do tipo não encontrado no índice".to_owned());
    };
    let top_level = file.symbols.iter().any(|symbol| {
        symbol.kind.is_type()
            && symbol.container.is_none()
            && crate::semantic::type_fqn(file, symbol) == fqn
    });
    if !top_level {
        return Err("mover apenas tipos top-level".to_owned());
    }
    let top_level_types = file
        .symbols
        .iter()
        .filter(|symbol| symbol.kind.is_type() && symbol.container.is_none())
        .count();
    if top_level_types > 1 {
        return Err(format!(
            "{} contém outros tipos top-level; mova-os separadamente",
            location.path.display()
        ));
    }
    let source = read_source(&location.path)
        .ok_or_else(|| format!("não foi possível ler {}", location.path.display()))?;
    let Some((package_start, package_end)) = file.package_range else {
        return Err("arquivo sem declaração de pacote".to_owned());
    };
    if package_end > source.len() {
        return Err("faixa de pacote inválida".to_owned());
    }

    let root = source_root_for(&location.path, project);
    let new_path = root
        .join(new_package.replace('.', std::path::MAIN_SEPARATOR_STR))
        .join(location.path.file_name().unwrap_or_default());
    if new_path != location.path && new_path.exists() {
        return Err(format!("o destino {} já existe", new_path.display()));
    }

    let mut files: Vec<FileEdits> = Vec::new();
    push_edit(
        &mut files,
        &location.path,
        package_start,
        package_end,
        &format!("package {new_package};"),
    );
    let references = find_type_references(fqn, project_types, index, classpath);
    let referred_files: std::collections::HashSet<PathBuf> = references
        .iter()
        .map(|reference| reference.file.clone())
        .collect();

    for (path, current) in &index.files {
        let Some(current_source) = read_source(path) else {
            continue;
        };
        for (import_index, import) in current.imports.iter().enumerate() {
            let import_path = import.strip_prefix("static ").unwrap_or(import).trim();
            let matches = import_path == fqn
                || import_path.starts_with(&format!("{fqn}."))
                || static_import_owner(import) == Some(fqn);
            if !matches {
                continue;
            }
            let Some((start, end)) = current.import_ranges.get(import_index).copied() else {
                continue;
            };
            if end <= start || end > current_source.len() {
                continue;
            }
            let raw = &current_source[start..end];
            if let Some(offset) = raw.find(old_package) {
                push_edit(
                    &mut files,
                    path,
                    start + offset,
                    start + offset + old_package.len(),
                    new_package,
                );
            }
        }
        for reference in &current.scoped_type_refs {
            if reference.name == fqn || reference.name.starts_with(&format!("{fqn}.")) {
                let start = reference.start_byte;
                if start < current_source.len() && current_source[start..].starts_with(old_package)
                {
                    push_edit(
                        &mut files,
                        path,
                        start,
                        start + old_package.len(),
                        new_package,
                    );
                }
            }
        }
        if path
            .file_name()
            .is_some_and(|name| name == "module-info.java")
        {
            for (start, _) in module_info_fqn_occurrences(&current_source, fqn) {
                push_edit(
                    &mut files,
                    path,
                    start,
                    start + old_package.len(),
                    new_package,
                );
            }
        }
        for (start, _) in javadoc_fqn_occurrences(&current_source, fqn) {
            push_edit(
                &mut files,
                path,
                start,
                start + old_package.len(),
                new_package,
            );
        }
    }

    for path in &referred_files {
        if *path == location.path {
            continue;
        }
        if path
            .file_name()
            .is_some_and(|name| name == "module-info.java")
        {
            continue;
        }
        let Some(target) = index.files.get(path) else {
            continue;
        };
        if target.package.as_deref() == Some(new_package) {
            continue;
        }
        let already_imported = target.imports.iter().any(|import| {
            let import_path = import.strip_prefix("static ").unwrap_or(import).trim();
            import_path == new_fqn
                || import_path == format!("{new_package}.*")
                || import_path == fqn
        });
        if already_imported {
            continue;
        }
        if target.symbols.iter().any(|symbol| {
            symbol.kind.is_type() && symbol.name == simple_name && symbol.container.is_none()
        }) {
            return Err(format!(
                "{} já declara um tipo {simple_name}; import automático cancelado",
                path.display()
            ));
        }
        if target
            .imports
            .iter()
            .any(|import| import.ends_with(&format!(".{simple_name}")) && import.as_str() != fqn)
        {
            return Err(format!(
                "{} já importa outro {simple_name}; import automático cancelado",
                path.display()
            ));
        }
        let Some(target_source) = read_source(path) else {
            continue;
        };
        let insert_at = target
            .import_ranges
            .last()
            .map(|(_, end)| *end)
            .or_else(|| target.package_range.map(|(_, end)| end))
            .unwrap_or(0);
        if insert_at > target_source.len() {
            continue;
        }
        let prefix = if target.import_ranges.is_empty() {
            "\n\n"
        } else {
            "\n"
        };
        push_edit(
            &mut files,
            path,
            insert_at,
            insert_at,
            &format!("{prefix}import {new_fqn};"),
        );
    }

    files.retain(|file| !file.edits.is_empty());
    for file in &mut files {
        file.edits
            .sort_by_key(|edit| std::cmp::Reverse(edit.start_byte));
        file.edits
            .dedup_by(|left, right| left.start_byte == right.start_byte);
    }
    if files.is_empty() {
        return Err("nenhuma ocorrência encontrada".to_owned());
    }
    let mut warnings = rename_warnings();
    warnings.push(format!(
        "Tipo movido de {old_package} para {new_package}; revise strings, recursos e referências por reflexão."
    ));
    Ok(RenamePlan {
        old_name: fqn.to_owned(),
        new_name: new_fqn,
        kind: RenameKind::MoveType,
        files,
        moves: if new_path != location.path {
            vec![(location.path.clone(), new_path)]
        } else {
            Vec::new()
        },
        warnings,
    })
}

/// Plan a package rename/move. Supports the package itself and its direct and
/// nested subpackages: package declarations, imports, qualified type refs and
/// `.java` file locations are updated.
pub fn plan_package_rename(
    old_package: &str,
    new_package: &str,
    project: &Project,
    index: &ProjectIndex,
    classpath: &ClasspathIndex,
) -> Result<RenamePlan, String> {
    if old_package.trim().is_empty() {
        return Err("rename do pacote default não é suportado".to_owned());
    }
    if new_package.trim().is_empty() {
        return Err("informe o novo pacote".to_owned());
    }
    if old_package == new_package {
        return Err("o novo pacote é igual ao atual".to_owned());
    }
    for segment in new_package.split('.') {
        validate_identifier(segment)?;
    }
    if new_package == "java"
        || new_package.starts_with("java.")
        || new_package.starts_with("javax.")
    {
        return Err("não é permitido renomear para o namespace java/javax".to_owned());
    }
    if classpath.package_exists(new_package)
        && !index.files.values().any(|file| {
            file.package
                .as_deref()
                .is_some_and(|package| package == new_package)
        })
    {
        return Err(format!(
            "já existe o pacote {new_package} no classpath; rename cancelado"
        ));
    }

    let mut files: Vec<FileEdits> = Vec::new();
    let mut moves: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut targets = std::collections::HashSet::new();
    let mut moved_files = 0;
    for (path, file) in &index.files {
        let Some(package) = file.package.as_deref() else {
            continue;
        };
        if package != old_package && !package.starts_with(&format!("{old_package}.")) {
            continue;
        }
        let suffix = package.strip_prefix(old_package).unwrap_or("");
        let new_package_for_file = format!("{new_package}{suffix}");
        let Some((start, end)) = file.package_range else {
            return Err(format!(
                "não foi possível localizar a declaração de pacote em {}",
                path.display()
            ));
        };
        let Some(source) = read_source(path) else {
            return Err(format!("não foi possível ler {}", path.display()));
        };
        if end > source.len() || start >= end {
            return Err(format!("faixa de pacote inválida em {}", path.display()));
        }
        let raw = &source[start..end];
        let Some(offset) = raw.find(package) else {
            return Err(format!(
                "declaração de pacote inesperada em {}",
                path.display()
            ));
        };
        push_edit(
            &mut files,
            path,
            start + offset,
            start + offset + package.len(),
            &new_package_for_file,
        );

        let root = source_root_for(path, project);
        let new_path = root
            .join(new_package_for_file.replace('.', std::path::MAIN_SEPARATOR_STR))
            .join(path.file_name().unwrap_or_default());
        if new_path != *path {
            if new_path.exists() || !targets.insert(new_path.clone()) {
                return Err(format!(
                    "o destino {} já existe ou colide com outro arquivo",
                    new_path.display()
                ));
            }
            moves.push((path.clone(), new_path));
            moved_files += 1;
        }
    }
    if files.is_empty() {
        return Err(format!("nenhum arquivo encontrado no pacote {old_package}"));
    }

    let old_prefix = format!("{old_package}.");
    for (path, file) in &index.files {
        let needs_imports = file.imports.iter().any(|import| {
            let path = import.strip_prefix("static ").unwrap_or(import).trim();
            path == old_package || path.starts_with(&old_prefix)
        });
        let needs_scoped = file
            .scoped_type_refs
            .iter()
            .any(|reference| reference.name.starts_with(&old_prefix));
        if !needs_imports && !needs_scoped {
            continue;
        }
        let Some(source) = read_source(path) else {
            continue;
        };
        if needs_imports {
            for (index, import) in file.imports.iter().enumerate() {
                let import_path = import.strip_prefix("static ").unwrap_or(import).trim();
                if import_path != old_package && !import_path.starts_with(&old_prefix) {
                    continue;
                }
                let (start, end) = file.import_ranges.get(index).copied().unwrap_or((0, 0));
                if end <= start || end > source.len() {
                    continue;
                }
                let raw = &source[start..end];
                if let Some(offset) = raw.find(old_package) {
                    push_edit(
                        &mut files,
                        path,
                        start + offset,
                        start + offset + old_package.len(),
                        new_package,
                    );
                }
            }
        }
        for reference in &file.scoped_type_refs {
            if reference.name.starts_with(&old_prefix) {
                push_edit(
                    &mut files,
                    path,
                    reference.start_byte,
                    reference.start_byte + old_package.len(),
                    new_package,
                );
            }
        }
    }
    for path in index.files.keys() {
        let Some(source) = read_source(path) else {
            continue;
        };
        if path
            .file_name()
            .is_some_and(|name| name == "module-info.java")
        {
            for (start, length) in module_info_package_occurrences(&source, old_package) {
                push_edit(&mut files, path, start, start + length, new_package);
            }
        }
        for (start, length) in javadoc_package_occurrences(&source, old_package) {
            push_edit(&mut files, path, start, start + length, new_package);
        }
    }

    files.retain(|file| !file.edits.is_empty());
    for file in &mut files {
        file.edits
            .sort_by_key(|edit| std::cmp::Reverse(edit.start_byte));
        file.edits
            .dedup_by(|left, right| left.start_byte == right.start_byte);
    }
    let mut warnings = rename_warnings();
    warnings.push(format!(
        "Pacote renomeado com {moved_files} arquivo(s) movido(s); revise strings e referências por reflexão."
    ));
    Ok(RenamePlan {
        old_name: old_package.to_owned(),
        new_name: new_package.to_owned(),
        kind: RenameKind::Package,
        files,
        moves,
        warnings,
    })
}

fn source_root_for(path: &Path, project: &Project) -> PathBuf {
    project
        .source_roots
        .iter()
        .filter(|root| path.starts_with(root))
        .max_by_key(|root| root.as_os_str().len())
        .cloned()
        .unwrap_or_else(|| project.root.clone())
}

/// Plan an import organization: sort normal imports, then static imports,
/// remove exact duplicates and keep each declaration in its original line.
pub fn organize_imports(source: &str, file: &FileSymbols) -> Result<Vec<TextEdit>, String> {
    if file.imports.is_empty() {
        return Err("o arquivo não possui imports".to_owned());
    }
    if file.import_ranges.len() != file.imports.len() {
        return Err(
            "índice de imports inconsistente; salve o arquivo e tente novamente".to_owned(),
        );
    }
    let mut normal = Vec::new();
    let mut statics = Vec::new();
    for import in &file.imports {
        let import = import.trim();
        if let Some(static_import) = import.strip_prefix("static ") {
            statics.push(format!("import static {};", static_import.trim()));
        } else {
            normal.push(format!("import {};", import));
        }
    }
    normal.sort();
    normal.dedup();
    statics.sort();
    statics.dedup();
    let mut ordered = normal;
    ordered.extend(statics);
    let mut edits = Vec::new();
    for (index, (start, end)) in file.import_ranges.iter().enumerate() {
        if *end > source.len() || start >= end {
            return Err("faixa de import inválida".to_owned());
        }
        let replacement = ordered.get(index).cloned().unwrap_or_default();
        edits.push(TextEdit {
            start_byte: *start,
            end_byte: *end,
            replacement,
        });
    }
    edits.sort_by(|left, right| right.start_byte.cmp(&left.start_byte));
    Ok(edits)
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
            .sort_by_key(|edit| std::cmp::Reverse(edit.start_byte));
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
        moves: Vec::new(),
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
    edits.sort_by_key(|edit| std::cmp::Reverse(edit.start_byte));
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
    edits.sort_by_key(|edit| std::cmp::Reverse(edit.start_byte));
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

fn type_name_occurrences(haystack: &str, needle: &str) -> Vec<(usize, usize)> {
    identifier_occurrences(haystack, needle)
        .into_iter()
        .filter(|(start, length)| !haystack[start + length..].starts_with('.'))
        .collect()
}

fn package_occurrences(haystack: &str, package: &str) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut start = 0;
    while let Some(position) = haystack[start..].find(package) {
        let absolute = start + position;
        let end = absolute + package.len();
        let left_ok = haystack[..absolute]
            .chars()
            .next_back()
            .is_none_or(|character| !is_identifier_char(character) && character != '.');
        let right_ok = haystack[end..]
            .chars()
            .next()
            .is_none_or(|character| !is_identifier_char(character));
        if left_ok && right_ok {
            found.push((absolute, package.len()));
        }
        start = end;
    }
    found
}

fn javadoc_ranges(source: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut index = 0;
    while let Some(position) = source[index..].find("/**") {
        let start = index + position;
        let body = start + 3;
        let Some(end_position) = source[body..].find("*/") else {
            break;
        };
        let end = body + end_position;
        ranges.push((body, end));
        index = end + 2;
    }
    ranges
}

fn javadoc_text_occurrences(source: &str, name: &str) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    for (start, end) in javadoc_ranges(source) {
        for (offset, length) in type_name_occurrences(&source[start..end], name) {
            found.push((start + offset, length));
        }
    }
    found
}

fn javadoc_package_occurrences(source: &str, package: &str) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    for (start, end) in javadoc_ranges(source) {
        for (offset, length) in package_occurrences(&source[start..end], package) {
            found.push((start + offset, length));
        }
    }
    found
}

fn fqn_occurrences(haystack: &str, fqn: &str) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut start = 0;
    while let Some(position) = haystack[start..].find(fqn) {
        let absolute = start + position;
        let end = absolute + fqn.len();
        let left_ok = haystack[..absolute]
            .chars()
            .next_back()
            .is_none_or(|character| !is_identifier_char(character) && character != '.');
        let right_ok = haystack[end..]
            .chars()
            .next()
            .is_none_or(|character| !is_identifier_char(character));
        if left_ok && right_ok {
            found.push((absolute, fqn.len()));
        }
        start = end;
    }
    found
}

fn javadoc_fqn_occurrences(source: &str, fqn: &str) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    for (start, end) in javadoc_ranges(source) {
        for (offset, length) in fqn_occurrences(&source[start..end], fqn) {
            found.push((start + offset, length));
        }
    }
    found
}

fn module_info_fqn_occurrences(source: &str, fqn: &str) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let directive = ["uses ", "provides ", "with ", "permits "]
            .iter()
            .any(|prefix| trimmed.starts_with(prefix));
        if directive {
            for (start, length) in fqn_occurrences(line, fqn) {
                found.push((offset + start, length));
            }
        }
        offset += line.len();
    }
    found
}

fn module_info_type_occurrences(source: &str, name: &str, fqn: &str) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let directive = ["uses ", "provides ", "with ", "permits "]
            .iter()
            .any(|prefix| trimmed.starts_with(prefix));
        if directive {
            for (start, length) in type_name_occurrences(line, name) {
                let reference = &line[start..start + length];
                if reference == name || fqn.ends_with(&format!(".{name}")) || fqn == name {
                    found.push((offset + start, length));
                }
            }
        }
        offset += line.len();
    }
    found
}

fn module_info_package_occurrences(source: &str, package: &str) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let directive = ["exports ", "opens ", "uses ", "provides ", "with "]
            .iter()
            .any(|prefix| trimmed.starts_with(prefix));
        if directive {
            for (start, length) in package_occurrences(line, package) {
                found.push((offset + start, length));
            }
        }
        offset += line.len();
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
        "Strings, recursos e referências por reflexão não são atualizados; Javadoc e module-info são atualizados quando possível.".to_owned(),
        "Rename de workspace exige buffers Java salvos (buffers sujos bloqueiam).".to_owned(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_source;

    #[test]
    fn textual_edits_only_touch_strings_comments_and_words() {
        let source = "class App {\n    // count the items\n    String label = \"counted\";\n    String other = \"count\";\n    int count = other.length();\n    /* multi\n       count here */\n}\n";
        let edits = plan_textual_java_edits(source, "count", "total");
        // "count the items" (comment), "count" (string), "count here" (block).
        // "counted" and the identifier `count` are untouched.
        assert_eq!(edits.len(), 3);
        let updated = apply_text_edits(source, &edits).unwrap();
        assert!(updated.contains("// total the items"));
        assert!(updated.contains("\"total\""));
        assert!(updated.contains("total here"));
        assert!(updated.contains("\"counted\""));
        assert!(updated.contains("int count = other.length();"));
    }

    #[test]
    fn textual_resource_edits_match_words() {
        let text = "greeting=hello count\ncount.count=counted\n";
        let edits = plan_textual_resource_edits(text, "count", "total");
        assert_eq!(edits.len(), 3);
        let updated = apply_text_edits(text, &edits).unwrap();
        assert!(updated.contains("hello total"));
        assert!(updated.contains("total.total=counted") || updated.contains("counted"));
    }

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
            plan.moves.first().map(|(_, to)| to.clone()),
            Some(user_path.with_file_name("Account.java"))
        );
    }

    #[test]
    fn type_rename_updates_local_declaration_and_references() {
        let path = PathBuf::from("com/acme/App.java");
        let file = parse_source(
            &path,
            "package com.acme; class App { void run() { class Local { int value; } Local x = new Local(); } }",
        );
        let project = reclipse_core::Project {
            root: PathBuf::from("."),
            name: "test".to_owned(),
            build_system: reclipse_core::BuildSystem::Plain,
            source_roots: Vec::new(),
            java_files: vec![path.clone()],
            all_files: vec![path.clone()],
        };
        let index = ProjectIndex::from_files(project, vec![file], 0);
        let types = ProjectTypes::build(&index);
        let plan = plan_type_rename(
            "com.acme.App.Local",
            "Scoped",
            &types,
            &index,
            &ClasspathIndex::default(),
        )
        .unwrap();
        // Declaration + `Local x` + `new Local()`: no file move for locals.
        assert!(plan.total_edits() >= 3, "edits: {}", plan.total_edits());
        assert!(plan.moves.is_empty());
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
    fn organizes_imports_and_static_last() {
        let source =
            "package p;\nimport z.Z;\nimport static b.B.x;\nimport a.A;\nimport z.Z;\nclass C {}\n";
        let file = crate::parse_source(Path::new("C.java"), source);
        let edits = organize_imports(source, &file).unwrap();
        let updated = apply_text_edits(source, &edits).unwrap();
        let first = updated.find("import a.A;").unwrap();
        let second = updated.find("import z.Z;").unwrap();
        let static_pos = updated.find("import static b.B.x;").unwrap();
        assert!(first < second && second < static_pos);
        assert_eq!(updated.matches("import z.Z;").count(), 1);
    }

    #[test]
    fn package_rename_moves_files_and_updates_imports() {
        let dir = tempfile::tempdir().unwrap();
        let app_path = dir.path().join("src/com/acme/App.java");
        let service_path = dir.path().join("src/com/acme/service/Service.java");
        std::fs::create_dir_all(service_path.parent().unwrap()).unwrap();
        let app_source = "package com.acme; import com.acme.service.Service; class App { Service service; com.acme.service.Service other; }";
        let service_source = "package com.acme.service; public class Service {}";
        std::fs::write(&app_path, app_source).unwrap();
        std::fs::write(&service_path, service_source).unwrap();
        let project = reclipse_core::discover_project(dir.path()).unwrap();
        let index = crate::build_index(project.clone(), None);
        let types = ProjectTypes::build(&index);
        let plan = plan_package_rename(
            "com.acme",
            "org.demo",
            &project,
            &index,
            &ClasspathIndex::default(),
        )
        .unwrap();
        assert_eq!(plan.kind, RenameKind::Package);
        assert_eq!(plan.moves.len(), 2);
        for file in &plan.files {
            let text = std::fs::read_to_string(&file.path).unwrap();
            let updated = apply_text_edits(&text, &file.edits).unwrap();
            std::fs::write(&file.path, updated).unwrap();
        }
        for (from, to) in &plan.moves {
            std::fs::create_dir_all(to.parent().unwrap()).unwrap();
            std::fs::rename(from, to).unwrap();
        }
        let app_source = std::fs::read_to_string(dir.path().join("src/org/demo/App.java")).unwrap();
        assert!(app_source.contains("package org.demo;"));
        assert!(app_source.contains("import org.demo.service.Service;"));
        assert!(app_source.contains("org.demo.service.Service other"));
        let service_source =
            std::fs::read_to_string(dir.path().join("src/org/demo/service/Service.java")).unwrap();
        assert!(service_source.contains("package org.demo.service;"));
        let _ = types;
    }

    #[test]
    fn type_rename_updates_javadoc_and_module_info() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("src/main/java");
        let user_path = root.join("com/acme/User.java");
        let module_path = root.join("module-info.java");
        std::fs::create_dir_all(user_path.parent().unwrap()).unwrap();
        std::fs::write(
            &user_path,
            "package com.acme;\n/** Links to {@link User} and {@link com.acme.User}. */\npublic class User {}\n",
        )
        .unwrap();
        std::fs::write(
            &module_path,
            "module demo {\n    uses com.acme.User;\n    provides com.acme.Service with com.acme.User;\n}\n",
        )
        .unwrap();
        let project = reclipse_core::discover_project(dir.path()).unwrap();
        let index = crate::build_index(project.clone(), None);
        let types = ProjectTypes::build(&index);
        let plan = plan_type_rename(
            "com.acme.User",
            "Account",
            &types,
            &index,
            &ClasspathIndex::default(),
        )
        .unwrap();
        let user_source = std::fs::read_to_string(&user_path).unwrap();
        let updated = apply_text_edits(
            &user_source,
            &plan
                .files
                .iter()
                .find(|file| file.path == user_path)
                .map(|file| file.edits.clone())
                .unwrap_or_default(),
        )
        .unwrap();
        assert!(updated.contains("{@link Account}"));
        assert!(updated.contains("{@link com.acme.Account}"));
        let module_source = std::fs::read_to_string(&module_path).unwrap();
        let updated = apply_text_edits(
            &module_source,
            &plan
                .files
                .iter()
                .find(|file| file.path == module_path)
                .map(|file| file.edits.clone())
                .unwrap_or_default(),
        )
        .unwrap();
        assert!(updated.contains("uses com.acme.Account;"));
        assert!(updated.contains("with com.acme.Account;"));
    }

    #[test]
    fn package_rename_updates_javadoc_and_module_info() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("src/main/java");
        let user_path = root.join("com/acme/User.java");
        let module_path = root.join("module-info.java");
        std::fs::create_dir_all(user_path.parent().unwrap()).unwrap();
        std::fs::write(
            &user_path,
            "package com.acme;\n/** See {@link com.acme.User}. */\npublic class User {}\n",
        )
        .unwrap();
        std::fs::write(
            &module_path,
            "module demo {\n    exports com.acme;\n    uses com.acme.User;\n}\n",
        )
        .unwrap();
        let project = reclipse_core::discover_project(dir.path()).unwrap();
        let index = crate::build_index(project.clone(), None);
        let plan = plan_package_rename(
            "com.acme",
            "org.demo",
            &project,
            &index,
            &ClasspathIndex::default(),
        )
        .unwrap();
        let user_source = std::fs::read_to_string(&user_path).unwrap();
        let updated = apply_text_edits(
            &user_source,
            &plan
                .files
                .iter()
                .find(|file| file.path == user_path)
                .map(|file| file.edits.clone())
                .unwrap_or_default(),
        )
        .unwrap();
        assert!(updated.contains("{@link org.demo.User}"));
        let module_source = std::fs::read_to_string(&module_path).unwrap();
        let updated = apply_text_edits(
            &module_source,
            &plan
                .files
                .iter()
                .find(|file| file.path == module_path)
                .map(|file| file.edits.clone())
                .unwrap_or_default(),
        )
        .unwrap();
        assert!(updated.contains("exports org.demo;"));
        assert!(updated.contains("uses org.demo.User;"));
    }

    #[test]
    fn type_move_updates_package_imports_javadoc_and_module_info() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("src/main/java");
        let user_path = root.join("com/acme/User.java");
        let app_path = root.join("com/acme/App.java");
        let other_path = root.join("com/other/Other.java");
        let module_path = root.join("module-info.java");
        std::fs::create_dir_all(user_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(other_path.parent().unwrap()).unwrap();
        std::fs::write(&user_path, "package com.acme;\npublic class User {}\n").unwrap();
        std::fs::write(&app_path, "package com.acme;\nclass App { User user; }\n").unwrap();
        std::fs::write(
            &other_path,
            "package com.other;\nimport com.acme.User;\n/** See {@link com.acme.User}. */\nclass Other { User user; com.acme.User other; }\n",
        )
        .unwrap();
        std::fs::write(
            &module_path,
            "module demo {\n    uses com.acme.User;\n    provides com.acme.Service with com.acme.User;\n}\n",
        )
        .unwrap();
        let project = reclipse_core::discover_project(dir.path()).unwrap();
        let index = crate::build_index(project.clone(), None);
        let types = ProjectTypes::build(&index);
        let plan = plan_type_move(
            "com.acme.User",
            "org.demo",
            &project,
            &types,
            &index,
            &ClasspathIndex::default(),
        )
        .unwrap();
        assert_eq!(plan.kind, RenameKind::MoveType);
        assert_eq!(plan.moves.len(), 1);
        assert_eq!(plan.moves[0].1, root.join("org/demo/User.java"));
        let read = |path: &Path| std::fs::read_to_string(path).unwrap();
        let changed = |path: &Path| {
            let source = read(path);
            let edits = plan
                .files
                .iter()
                .find(|file| file.path == path)
                .map(|file| file.edits.clone())
                .unwrap_or_default();
            apply_text_edits(&source, &edits).unwrap()
        };
        assert!(changed(&user_path).contains("package org.demo;"));
        let app = changed(&app_path);
        assert!(app.contains("import org.demo.User;"), "app: {app}");
        let other = changed(&other_path);
        assert!(other.contains("import org.demo.User;"), "other: {other}");
        assert!(other.contains("org.demo.User other"), "other: {other}");
        assert!(other.contains("{@link org.demo.User}"), "other: {other}");
        let module = changed(&module_path);
        assert!(module.contains("uses org.demo.User;"), "module: {module}");
        assert!(module.contains("with org.demo.User;"), "module: {module}");
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
        let (from, to) = plan.moves[0].clone();
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
    fn ambiguous_static_imports_do_not_produce_references() {
        let dir = tempfile::tempdir().unwrap();
        let a_path = dir.path().join("com/acme/A.java");
        let b_path = dir.path().join("com/acme/B.java");
        let app_path = dir.path().join("com/acme/App.java");
        std::fs::create_dir_all(a_path.parent().unwrap()).unwrap();
        let a_source = "package com.acme; public class A { public static void save() {} }";
        let b_source = "package com.acme; public class B { public static void save() {} }";
        let app_source = "package com.acme; import static com.acme.A.save; import static com.acme.B.save; class App { void run() { save(); } }";
        std::fs::write(&a_path, a_source).unwrap();
        std::fs::write(&b_path, b_source).unwrap();
        std::fs::write(&app_path, app_source).unwrap();
        let a = parse_source(&a_path, a_source);
        let b = parse_source(&b_path, b_source);
        let app = parse_source(&app_path, app_source);
        let project = reclipse_core::Project {
            root: dir.path().to_path_buf(),
            name: "test".to_owned(),
            build_system: reclipse_core::BuildSystem::Plain,
            source_roots: Vec::new(),
            java_files: vec![a_path.clone(), b_path, app_path],
            all_files: Vec::new(),
        };
        let index = ProjectIndex::from_files(project, vec![a, b, app], 0);
        let types = ProjectTypes::build(&index);
        let target = MemberTarget::Project {
            path: a_path,
            container: "A".to_owned(),
            type_fqn: "com.acme.A".to_owned(),
            name: "save".to_owned(),
            is_call: true,
        };
        assert!(
            find_member_references(&target, &types, &index, &ClasspathIndex::default()).is_empty()
        );
    }

    #[test]
    fn type_rename_refuses_when_a_local_type_shadows_the_name() {
        let dir = tempfile::tempdir().unwrap();
        let user_path = dir.path().join("com/acme/User.java");
        let app_path = dir.path().join("com/acme/App.java");
        std::fs::create_dir_all(user_path.parent().unwrap()).unwrap();
        let user_source = "package com.acme; public class User {}";
        let app_source =
            "package com.acme; class App { void run() { class User {} User local = new User(); } }";
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
            all_files: vec![user_path, app_path],
        };
        let index = ProjectIndex::from_files(project, vec![user, app], 0);
        let types = ProjectTypes::build(&index);
        let error = plan_type_rename(
            "com.acme.User",
            "Account",
            &types,
            &index,
            &ClasspathIndex::default(),
        )
        .unwrap_err();
        assert!(error.contains("tipo local"));
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
