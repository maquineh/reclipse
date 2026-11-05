//! Fast, error-tolerant Java syntax indexing built on tree-sitter.
//!
//! This is a *syntax* index: packages, imports, type declarations, methods,
//! fields and syntax errors. Semantic resolution (types, classpath, overloads)
//! belongs to the next milestone.

use std::path::{Path, PathBuf};

use tree_sitter::{Node, Parser};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Class,
    Interface,
    Enum,
    Record,
    Annotation,
    Method,
    Constructor,
    Field,
    EnumConstant,
}

impl SymbolKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Class => "class",
            Self::Interface => "interface",
            Self::Enum => "enum",
            Self::Record => "record",
            Self::Annotation => "annotation",
            Self::Method => "method",
            Self::Constructor => "constructor",
            Self::Field => "field",
            Self::EnumConstant => "constant",
        }
    }

    pub fn is_type(self) -> bool {
        matches!(
            self,
            Self::Class | Self::Interface | Self::Enum | Self::Record | Self::Annotation
        )
    }

    pub fn glyph(self) -> &'static str {
        match self {
            Self::Class => "C",
            Self::Interface => "I",
            Self::Enum => "E",
            Self::Record => "R",
            Self::Annotation => "@",
            Self::Method => "m",
            Self::Constructor => "c",
            Self::Field => "f",
            Self::EnumConstant => "k",
        }
    }
}

#[derive(Debug, Clone)]
pub struct JavaSymbol {
    pub name: String,
    pub kind: SymbolKind,
    /// 1-based line.
    pub line: usize,
    /// 1-based column (in characters, as reported by tree-sitter).
    pub column: usize,
    /// Dot-separated enclosing type chain, e.g. `Outer.Inner`.
    pub container: Option<String>,
    /// Nesting depth used by the outline view.
    pub depth: usize,
    /// Short declaration head, e.g. `public static void main(String[] args)`.
    pub detail: Option<String>,
    /// Byte range of the symbol name (not the whole declaration head).
    pub name_start_byte: usize,
    pub name_end_byte: usize,
}

/// A member (method/field/constant) occurrence in source code.
///
/// `qualifier` is the receiver text before the dot (`app.service`), or `None`
/// for unqualified references. `container` is the enclosing type chain, used to
/// resolve unqualified members.
#[derive(Debug, Clone)]
pub struct JavaMemberReference {
    pub name: String,
    pub qualifier: Option<String>,
    pub container: Option<String>,
    /// True for method calls/references, false for fields/constants.
    pub is_call: bool,
    /// 1-based line.
    pub line: usize,
    /// 1-based column.
    pub column: usize,
    /// Byte offsets in the original source.
    pub start_byte: usize,
    pub end_byte: usize,
}

/// Raw `extends`/`implements` information for a declared type. Names keep the
/// spelling used in the file (simple or qualified) and are resolved later with
/// the declaring file's imports.
#[derive(Debug, Clone)]
pub struct JavaTypeRelation {
    pub type_name: String,
    pub super_types: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ParseError {
    /// 1-based line.
    pub line: usize,
    /// 1-based column.
    pub column: usize,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct JavaReference {
    pub name: String,
    /// 1-based line.
    pub line: usize,
    /// 1-based column.
    pub column: usize,
    /// Byte offsets in the original source.
    pub start_byte: usize,
    pub end_byte: usize,
}

#[derive(Debug, Clone)]
pub struct FileSymbols {
    pub path: PathBuf,
    pub package: Option<String>,
    pub imports: Vec<String>,
    pub import_lines: Vec<usize>,
    /// Byte ranges of each import declaration (same order as `imports`).
    pub import_ranges: Vec<(usize, usize)>,
    pub symbols: Vec<JavaSymbol>,
    /// Type-identifier occurrences (including declarations) for semantic refs.
    pub type_refs: Vec<JavaReference>,
    /// Method/field occurrences used by member references and rename.
    pub member_refs: Vec<JavaMemberReference>,
    /// Raw superclass/interface names declared in this file.
    pub type_relations: Vec<JavaTypeRelation>,
    /// Names of local (method-scoped) type declarations. They are not project
    /// types and must not be confused with same-named top-level types.
    pub local_type_names: Vec<String>,
    pub errors: Vec<ParseError>,
    pub line_count: usize,
    /// Set when the file could not be read or the parser itself failed.
    pub parse_error: Option<String>,
}

impl FileSymbols {
    pub fn empty(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            package: None,
            imports: Vec::new(),
            import_lines: Vec::new(),
            import_ranges: Vec::new(),
            symbols: Vec::new(),
            type_refs: Vec::new(),
            member_refs: Vec::new(),
            type_relations: Vec::new(),
            local_type_names: Vec::new(),
            errors: Vec::new(),
            line_count: 0,
            parse_error: None,
        }
    }
}

/// Parse a Java source file with a fresh parser.
pub fn parse_source(path: &Path, source: &str) -> FileSymbols {
    let mut parser = Parser::new();
    parse_source_with(&mut parser, path, source)
}

/// Parse using a reusable parser (one per worker thread).
pub fn parse_source_with(parser: &mut Parser, path: &Path, source: &str) -> FileSymbols {
    let mut file = FileSymbols::empty(path);
    file.line_count = source.split('\n').count().max(1);

    let language: tree_sitter::Language = tree_sitter_java::LANGUAGE.into();
    if let Err(error) = parser.set_language(&language) {
        file.parse_error = Some(format!("failed to load Java grammar: {error}"));
        return file;
    }

    let Some(tree) = parser.parse(source, None) else {
        file.parse_error = Some("tree-sitter returned no syntax tree".to_owned());
        return file;
    };

    let root = tree.root_node();
    let mut container = Vec::new();
    visit_children(root, source.as_bytes(), &mut file, &mut container, 0);
    visit_type_refs(root, source.as_bytes(), &mut file);
    let mut member_container = Vec::new();
    visit_member_refs(root, source.as_bytes(), &mut file, &mut member_container);

    if file.errors.len() > 200 {
        file.errors.truncate(200);
    }

    file
}

fn visit_children(
    node: Node<'_>,
    source: &[u8],
    file: &mut FileSymbols,
    container: &mut Vec<String>,
    depth: usize,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_node(child, source, file, container, depth);
    }
}

fn visit_type_refs(node: Node<'_>, source: &[u8], file: &mut FileSymbols) {
    if node.kind() == "type_identifier" {
        push_type_ref(node, source, file);
    } else if matches!(
        node.kind(),
        "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration"
    ) {
        if let Some(name) = node.child_by_field_name("name") {
            if is_local_type_declaration(node) {
                let name = text(name, source);
                if !name.is_empty() {
                    file.local_type_names.push(name);
                }
            } else {
                push_type_ref(name, source, file);
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_type_refs(child, source, file);
    }
}

fn is_local_type_declaration(node: Node<'_>) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "block"
            | "for_statement"
            | "enhanced_for_statement"
            | "lambda_expression"
            | "switch_block_statement_group" => return true,
            "class_body"
            | "interface_body"
            | "enum_body"
            | "annotation_type_body"
            | "record_body" => return false,
            _ => current = parent.parent(),
        }
    }
    false
}

fn push_type_ref(node: Node<'_>, source: &[u8], file: &mut FileSymbols) {
    let name = text(node, source);
    if name.is_empty() {
        return;
    }
    let position = node.start_position();
    file.type_refs.push(JavaReference {
        name,
        line: position.row + 1,
        column: position.column + 1,
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
    });
}

fn visit_member_refs(
    node: Node<'_>,
    source: &[u8],
    file: &mut FileSymbols,
    container: &mut Vec<String>,
) {
    let is_type = matches!(
        node.kind(),
        "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration"
    );
    let type_name = is_type
        .then(|| node.child_by_field_name("name"))
        .flatten()
        .map(|name| text(name, source))
        .filter(|name| !name.is_empty());

    if node.kind() == "identifier" {
        if let Some(reference) = classify_member_reference(node, source, container) {
            file.member_refs.push(reference);
        }
    }

    if let Some(name) = &type_name {
        container.push(name.clone());
    }
    let anonymous_body = node.kind() == "class_body"
        && node
            .parent()
            .is_some_and(|parent| parent.kind() == "object_creation_expression");
    if anonymous_body {
        container.push("<anonymous>".to_owned());
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_member_refs(child, source, file, container);
    }
    if anonymous_body {
        container.pop();
    }
    if type_name.is_some() {
        container.pop();
    }
}

fn classify_member_reference(
    node: Node<'_>,
    source: &[u8],
    container: &[String],
) -> Option<JavaMemberReference> {
    let name = text(node, source);
    if name.is_empty() {
        return None;
    }
    let parent = node.parent()?;
    if is_declaration_name(node, parent) {
        return None;
    }
    if matches!(
        parent.kind(),
        "package_declaration"
            | "import_declaration"
            | "scoped_identifier"
            | "inferred_parameters"
            | "labeled_statement"
            | "break_statement"
            | "continue_statement"
            | "marker_annotation"
            | "annotation"
            | "type_pattern"
            | "record_pattern"
            | "record_pattern_component"
            | "receiver_parameter"
    ) {
        return None;
    }
    if parent.kind() == "lambda_expression"
        && parent
            .child_by_field_name("parameters")
            .is_some_and(|parameters| parameters.id() == node.id())
    {
        return None;
    }

    let (qualifier, is_call) = match parent.kind() {
        "method_invocation" => {
            if parent
                .child_by_field_name("name")
                .is_none_or(|name_node| name_node.id() != node.id())
            {
                return None;
            }
            (
                parent
                    .child_by_field_name("object")
                    .map(|object| text(object, source)),
                true,
            )
        }
        "field_access" => {
            if parent
                .child_by_field_name("field")
                .is_none_or(|field| field.id() != node.id())
            {
                return None;
            }
            (
                parent
                    .child_by_field_name("object")
                    .map(|object| text(object, source)),
                false,
            )
        }
        "method_reference" => {
            let raw = text(parent, source);
            let before = raw.split("::").next().unwrap_or(&raw).trim();
            ((!before.is_empty()).then(|| before.to_owned()), true)
        }
        _ => (None, false),
    };

    let position = node.start_position();
    Some(JavaMemberReference {
        name,
        qualifier: qualifier.filter(|value| !value.is_empty()),
        container: container_name(container),
        is_call,
        line: position.row + 1,
        column: position.column + 1,
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
    })
}

fn is_declaration_name(node: Node<'_>, parent: Node<'_>) -> bool {
    let is_name = parent
        .child_by_field_name("name")
        .is_some_and(|name| name.id() == node.id());
    if !is_name {
        return false;
    }
    matches!(
        parent.kind(),
        "variable_declarator"
            | "formal_parameter"
            | "catch_formal_parameter"
            | "enhanced_for_statement"
            | "method_declaration"
            | "constructor_declaration"
            | "compact_constructor_declaration"
            | "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration"
            | "enum_constant"
            | "annotation_type_element_declaration"
            | "resource"
    )
}

fn visit_node(
    node: Node<'_>,
    source: &[u8],
    file: &mut FileSymbols,
    container: &mut Vec<String>,
    depth: usize,
) {
    if node.is_error() {
        push_error(node, file, "syntax error");
        return;
    }
    if node.is_missing() {
        push_error(node, file, &format!("missing `{}`", node.kind()));
        return;
    }

    match node.kind() {
        "package_declaration" => {
            if file.package.is_none() {
                file.package = node
                    .child_by_field_name("name")
                    .map(|name| text(name, source))
                    .filter(|name| !name.is_empty())
                    .or_else(|| {
                        Some(
                            text(node, source)
                                .trim()
                                .trim_start_matches("package")
                                .trim_end_matches(';')
                                .trim()
                                .to_owned(),
                        )
                    });
            }
        }
        "import_declaration" => {
            let raw = text(node, source);
            let import = raw
                .trim()
                .trim_start_matches("import")
                .trim_end_matches(';')
                .trim()
                .to_owned();
            if !import.is_empty() {
                file.imports.push(import);
                file.import_lines.push(node.start_position().row + 1);
                file.import_ranges
                    .push((node.start_byte(), node.end_byte()));
            }
        }
        "class_declaration"
        | "interface_declaration"
        | "enum_declaration"
        | "annotation_type_declaration" => {
            let kind = match node.kind() {
                "class_declaration" => SymbolKind::Class,
                "interface_declaration" => SymbolKind::Interface,
                "enum_declaration" => SymbolKind::Enum,
                _ => SymbolKind::Annotation,
            };
            push_type(node, source, file, container, depth, kind, false);
        }
        "record_declaration" => {
            push_type(
                node,
                source,
                file,
                container,
                depth,
                SymbolKind::Record,
                true,
            );
        }
        "method_declaration" | "constructor_declaration" | "compact_constructor_declaration" => {
            let Some(name_node) = node.child_by_field_name("name") else {
                return;
            };
            let name = text(name_node, source);
            if name.is_empty() {
                return;
            }
            let kind = if node.kind() == "method_declaration" {
                SymbolKind::Method
            } else {
                SymbolKind::Constructor
            };
            file.symbols.push(JavaSymbol {
                name,
                kind,
                line: node.start_position().row + 1,
                column: node.start_position().column + 1,
                container: container_name(container),
                depth,
                detail: Some(declaration_head(node, source)),
                name_start_byte: name_node.start_byte(),
                name_end_byte: name_node.end_byte(),
            });
        }
        "field_declaration" | "constant_declaration" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "variable_declarator" {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        let name = text(name_node, source);
                        if !name.is_empty() {
                            file.symbols.push(JavaSymbol {
                                name,
                                kind: SymbolKind::Field,
                                line: child.start_position().row + 1,
                                column: child.start_position().column + 1,
                                container: container_name(container),
                                depth,
                                detail: Some(declaration_head(node, source)),
                                name_start_byte: name_node.start_byte(),
                                name_end_byte: name_node.end_byte(),
                            });
                        }
                    }
                }
            }
        }
        "block" | "static_initializer" => {
            // Local classes are not project symbols. Entering blocks would also
            // misattribute local members to the enclosing type.
        }
        "enum_constant" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = text(name_node, source);
                if !name.is_empty() {
                    file.symbols.push(JavaSymbol {
                        name,
                        kind: SymbolKind::EnumConstant,
                        line: node.start_position().row + 1,
                        column: node.start_position().column + 1,
                        container: container_name(container),
                        depth,
                        detail: None,
                        name_start_byte: name_node.start_byte(),
                        name_end_byte: name_node.end_byte(),
                    });
                }
            }
        }
        _ => {
            // Walk structural nodes only. Bodies are entered explicitly by
            // `push_type`, which avoids indexing locals as fields/methods.
            visit_children(node, source, file, container, depth);
        }
    }
}

fn push_type(
    node: Node<'_>,
    source: &[u8],
    file: &mut FileSymbols,
    container: &mut Vec<String>,
    depth: usize,
    kind: SymbolKind,
    is_record: bool,
) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = text(name_node, source);
    if name.is_empty() {
        return;
    }

    file.symbols.push(JavaSymbol {
        name: name.clone(),
        kind,
        line: node.start_position().row + 1,
        column: node.start_position().column + 1,
        container: container_name(container),
        depth,
        detail: Some(declaration_head(node, source)),
        name_start_byte: name_node.start_byte(),
        name_end_byte: name_node.end_byte(),
    });

    if let Some(relation) = type_relation(node, source, container, &name) {
        file.type_relations.push(relation);
    }

    container.push(name);
    let _ = is_record;

    if is_record {
        if let Some(parameters) = node.child_by_field_name("parameters") {
            let mut cursor = parameters.walk();
            for child in parameters.children(&mut cursor) {
                if child.kind() == "formal_parameter" {
                    if let Some(field_name) = child.child_by_field_name("name") {
                        let name = text(field_name, source);
                        if !name.is_empty() {
                            file.symbols.push(JavaSymbol {
                                name,
                                kind: SymbolKind::Field,
                                line: child.start_position().row + 1,
                                column: child.start_position().column + 1,
                                container: container_name(container),
                                depth: depth + 1,
                                detail: Some(declaration_head(child, source)),
                                name_start_byte: field_name.start_byte(),
                                name_end_byte: field_name.end_byte(),
                            });
                        }
                    }
                }
            }
        }
    }

    if let Some(body) = node.child_by_field_name("body") {
        visit_children(body, source, file, container, depth + 1);
    }
    container.pop();
}

fn type_relation(
    node: Node<'_>,
    source: &[u8],
    container: &[String],
    name: &str,
) -> Option<JavaTypeRelation> {
    let mut super_types = Vec::new();
    if let Some(superclass) = node.child_by_field_name("superclass") {
        if let Some(raw) = type_list_first(superclass, source) {
            super_types.push(raw);
        }
    }
    if let Some(interfaces) = node.child_by_field_name("interfaces") {
        super_types.extend(type_list_children(interfaces, source));
    }
    // Interface declarations use an unnamed `extends_interfaces` node.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "extends_interfaces" {
            super_types.extend(type_list_children(child, source));
        }
    }
    super_types.sort();
    super_types.dedup();
    if super_types.is_empty() {
        return None;
    }
    let mut type_name = container.to_vec();
    type_name.push(name.to_owned());
    Some(JavaTypeRelation {
        type_name: type_name.join("."),
        super_types,
    })
}

fn type_list_children(node: Node<'_>, source: &[u8]) -> Vec<String> {
    let mut cursor = node.walk();
    let mut types = Vec::new();
    for child in node.children(&mut cursor) {
        if child.kind() == "type_list" {
            let mut list_cursor = child.walk();
            for item in child.named_children(&mut list_cursor) {
                if let Some(raw) = raw_type_name(item, source) {
                    types.push(raw);
                }
            }
        }
    }
    types
}

fn type_list_first(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    let found = node
        .named_children(&mut cursor)
        .find_map(|child| raw_type_name(child, source));
    found
}

fn raw_type_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let raw = node.utf8_text(source).ok()?.trim();
    if raw.is_empty() || raw == "extends" || raw == "implements" {
        return None;
    }
    let raw = raw.split('<').next().unwrap_or(raw).trim();
    let raw = raw.trim_end_matches("[]").trim();
    if raw.is_empty() {
        None
    } else {
        Some(raw.to_owned())
    }
}

fn push_error(node: Node<'_>, file: &mut FileSymbols, message: &str) {
    let position = node.start_position();
    file.errors.push(ParseError {
        line: position.row + 1,
        column: position.column + 1,
        message: message.to_owned(),
    });
}

/// Byte range of a local variable/parameter name and the scope in which it is
/// visible. Used to avoid confusing locals with fields during reference search
/// and to power local rename.
#[derive(Debug, Clone)]
pub struct LocalBinding {
    pub name: String,
    pub kind: LocalBindingKind,
    pub name_start_byte: usize,
    pub name_end_byte: usize,
    pub declaration_start_byte: usize,
    pub scope_start_byte: usize,
    pub scope_end_byte: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalBindingKind {
    Variable,
    Parameter,
    CatchParameter,
    ForEach,
    Resource,
    LambdaParameter,
}

/// Parse local variable/parameter bindings and their scopes from source text.
pub fn parse_local_bindings(source: &str) -> Vec<LocalBinding> {
    let mut parser = Parser::new();
    let language: tree_sitter::Language = tree_sitter_java::LANGUAGE.into();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let mut bindings = Vec::new();
    collect_local_bindings(tree.root_node(), source.as_bytes(), &mut bindings);
    bindings.sort_by_key(|binding| (binding.scope_end_byte, binding.name_start_byte));
    bindings
}

fn collect_local_bindings(node: Node<'_>, source: &[u8], bindings: &mut Vec<LocalBinding>) {
    match node.kind() {
        "variable_declarator" => {
            if is_local_declarator(node) {
                if let (Some(name_node), Some(scope)) =
                    (node.child_by_field_name("name"), declaration_scope(node))
                {
                    push_binding(
                        bindings,
                        name_node,
                        source,
                        LocalBindingKind::Variable,
                        scope,
                    );
                }
            }
        }
        "formal_parameter" | "receiver_parameter" => {
            if let (Some(name_node), Some(scope)) =
                (node.child_by_field_name("name"), parameter_scope(node))
            {
                push_binding(
                    bindings,
                    name_node,
                    source,
                    LocalBindingKind::Parameter,
                    scope,
                );
            }
        }
        "catch_formal_parameter" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let scope = nearest_ancestor(node, &["catch_clause"]).unwrap_or(node);
                push_binding(
                    bindings,
                    name_node,
                    source,
                    LocalBindingKind::CatchParameter,
                    scope,
                );
            }
        }
        "enhanced_for_statement" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                push_binding(bindings, name_node, source, LocalBindingKind::ForEach, node);
            }
        }
        "resource" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let scope =
                    nearest_ancestor(node, &["try_with_resources_statement"]).unwrap_or(node);
                push_binding(
                    bindings,
                    name_node,
                    source,
                    LocalBindingKind::Resource,
                    scope,
                );
            }
        }
        "lambda_expression" => {
            if let Some(parameters) = node.child_by_field_name("parameters") {
                if parameters.kind() == "identifier" {
                    push_binding(
                        bindings,
                        parameters,
                        source,
                        LocalBindingKind::LambdaParameter,
                        node,
                    );
                }
            }
        }
        "inferred_parameters" => {
            let scope = nearest_ancestor(node, &["lambda_expression"]).unwrap_or(node);
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "identifier" {
                    push_binding(
                        bindings,
                        child,
                        source,
                        LocalBindingKind::LambdaParameter,
                        scope,
                    );
                }
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_local_bindings(child, source, bindings);
    }
}

fn push_binding(
    bindings: &mut Vec<LocalBinding>,
    name_node: Node<'_>,
    source: &[u8],
    kind: LocalBindingKind,
    scope: Node<'_>,
) {
    let name = name_node.utf8_text(source).unwrap_or("").trim().to_owned();
    if name.is_empty() {
        return;
    }
    bindings.push(LocalBinding {
        name,
        kind,
        name_start_byte: name_node.start_byte(),
        name_end_byte: name_node.end_byte(),
        declaration_start_byte: name_node.start_byte(),
        scope_start_byte: scope.start_byte(),
        scope_end_byte: scope.end_byte(),
    });
}

fn is_local_declarator(node: Node<'_>) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "block" | "for_statement" | "switch_block_statement_group" | "lambda_expression" => {
                return true;
            }
            "class_body"
            | "interface_body"
            | "enum_body"
            | "annotation_type_body"
            | "record_body"
            | "field_declaration"
            | "constant_declaration" => return false,
            _ => current = parent.parent(),
        }
    }
    false
}

fn declaration_scope(node: Node<'_>) -> Option<Node<'_>> {
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "for_statement"
            | "enhanced_for_statement"
            | "try_with_resources_statement"
            | "catch_clause"
            | "lambda_expression"
            | "block"
            | "switch_block_statement_group" => {
                return Some(parent);
            }
            "class_body"
            | "interface_body"
            | "enum_body"
            | "annotation_type_body"
            | "record_body" => return None,
            _ => current = parent.parent(),
        }
    }
    None
}

fn parameter_scope(node: Node<'_>) -> Option<Node<'_>> {
    nearest_ancestor(
        node,
        &[
            "method_declaration",
            "constructor_declaration",
            "lambda_expression",
        ],
    )
}

fn nearest_ancestor<'a>(node: Node<'a>, kinds: &[&str]) -> Option<Node<'a>> {
    let mut current = node.parent();
    while let Some(parent) = current {
        if kinds.contains(&parent.kind()) {
            return Some(parent);
        }
        current = parent.parent();
    }
    None
}

fn text(node: Node<'_>, source: &[u8]) -> String {
    node.utf8_text(source).unwrap_or("").trim().to_owned()
}

fn container_name(container: &[String]) -> Option<String> {
    if container.is_empty() {
        None
    } else {
        Some(container.join("."))
    }
}

fn declaration_head(node: Node<'_>, source: &[u8]) -> String {
    let raw = node.utf8_text(source).unwrap_or("");
    let head = raw.split('{').next().unwrap_or(raw);
    let head = head.split(';').next().unwrap_or(head);
    let mut line = head
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if line.chars().count() > 120 {
        line = line.chars().take(117).collect::<String>() + "...";
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
package com.acme.demo;

import java.util.List;
import static java.util.Objects.requireNonNull;

public class App {
    private final String name;
    public static final int VERSION = 1;

    public App(String name) {
        this.name = name;
    }

    public static void main(String[] args) {
        System.out.println("hello");
    }

    record Point(int x, int y) {}

    interface Greeter {
        String greet(String who);
    }

    enum Mode { FAST, SAFE }
}
"#;

    #[test]
    fn extracts_package_types_and_members() {
        let file = parse_source(Path::new("App.java"), SAMPLE);
        assert_eq!(file.package.as_deref(), Some("com.acme.demo"));
        assert!(file.imports.iter().any(|i| i.contains("java.util.List")));
        assert!(file.errors.is_empty(), "{:?}", file.errors);

        let names: Vec<_> = file.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"App"));
        assert!(names.contains(&"name"));
        assert!(names.contains(&"VERSION"));
        assert!(names.contains(&"App")); // constructor also named App
        assert!(names.contains(&"main"));
        assert!(names.contains(&"Point"));
        assert!(names.contains(&"x"));
        assert!(names.contains(&"Greeter"));
        assert!(names.contains(&"Mode"));
        assert!(names.iter().filter(|n| **n == "App").count() >= 2);

        let app = file
            .symbols
            .iter()
            .find(|s| s.name == "App" && s.kind == SymbolKind::Class)
            .unwrap();
        assert_eq!(app.container, None);
        assert_eq!(app.depth, 0);
        let main = file.symbols.iter().find(|s| s.name == "main").unwrap();
        assert_eq!(main.container.as_deref(), Some("App"));
        assert_eq!(main.depth, 1);
    }

    #[test]
    fn reports_syntax_errors() {
        let file = parse_source(
            Path::new("Broken.java"),
            "public class Broken { void x( { }",
        );
        assert!(!file.errors.is_empty());
    }

    #[test]
    fn collects_member_references_and_type_relations() {
        let source = r#"
package com.acme;
class Base { String name; void save() {} }
class App extends Base implements Runnable {
    App app;
    void run() {
        app.save();
        this.name = "x";
        Runnable r = Base::save;
        name.length();
    }
    public void run() {}
}
"#;
        let file = parse_source(Path::new("App.java"), source);
        let save: Vec<_> = file
            .member_refs
            .iter()
            .filter(|reference| reference.name == "save")
            .collect();
        assert!(save
            .iter()
            .any(|reference| reference.qualifier.as_deref() == Some("app") && reference.is_call));
        assert!(save.iter().any(|reference| {
            reference.qualifier.as_deref() == Some("Base") && reference.is_call
        }));
        assert!(file.member_refs.iter().any(|reference| {
            reference.name == "name"
                && reference.qualifier.as_deref() == Some("this")
                && !reference.is_call
        }));
        let relation = file
            .type_relations
            .iter()
            .find(|relation| relation.type_name == "App")
            .unwrap();
        assert!(relation.super_types.contains(&"Base".to_owned()));
        assert!(relation.super_types.contains(&"Runnable".to_owned()));
    }

    #[test]
    fn marks_anonymous_class_member_containers() {
        let source = r#"
class App {
    Runnable job = new Runnable() {
        public void run() { helper(); }
    };
    void helper() {}
}
"#;
        let file = parse_source(Path::new("App.java"), source);
        let helper = file
            .member_refs
            .iter()
            .find(|reference| reference.name == "helper" && reference.is_call)
            .unwrap();
        assert_eq!(helper.container.as_deref(), Some("App.<anonymous>"));
    }

    #[test]
    fn detects_local_type_declarations() {
        let source = r#"
class App {
    void run() {
        class Local { int value; }
        Local local = new Local();
    }
}
"#;
        let file = parse_source(Path::new("App.java"), source);
        assert_eq!(file.local_type_names, vec!["Local".to_owned()]);
        assert!(!file.symbols.iter().any(|symbol| symbol.name == "Local"));
    }

    #[test]
    fn collects_local_bindings_with_scopes() {
        let source = r#"
class App {
    void run(int parameter) {
        String name = "x";
        for (String item : java.util.List.of(name)) {
            name += item;
        }
        try (var reader = open()) { reader.read(); }
    }
}
"#;
        let bindings = parse_local_bindings(source);
        assert!(bindings
            .iter()
            .any(|binding| binding.name == "parameter"
                && binding.kind == LocalBindingKind::Parameter));
        assert!(bindings
            .iter()
            .any(|binding| binding.name == "name" && binding.kind == LocalBindingKind::Variable));
        assert!(bindings
            .iter()
            .any(|binding| binding.name == "item" && binding.kind == LocalBindingKind::ForEach));
        assert!(bindings
            .iter()
            .any(|binding| binding.name == "reader" && binding.kind == LocalBindingKind::Resource));
    }

    #[test]
    fn collects_type_identifier_references_inside_bodies() {
        let source = r#"
package com.acme;
import java.util.List;
class App {
    List<String> items;
    void run() { new User(); }
}
class User {}
"#;
        let file = parse_source(Path::new("App.java"), source);
        let names: Vec<&str> = file
            .type_refs
            .iter()
            .map(|reference| reference.name.as_str())
            .collect();
        assert!(names.contains(&"App"));
        assert!(names.contains(&"List"));
        assert!(names.contains(&"String"));
        assert!(names.contains(&"User"));
        assert!(names.iter().filter(|name| **name == "User").count() >= 2);
    }

    #[test]
    fn handles_unicode_and_text_blocks() {
        let source = r#"
class Café {
    String s = """
        olá
        mundo
        """;
}
"#;
        let file = parse_source(Path::new("Cafe.java"), source);
        assert!(file.symbols.iter().any(|s| s.name == "Café"));
        assert!(file.errors.is_empty(), "{:?}", file.errors);
    }
}
