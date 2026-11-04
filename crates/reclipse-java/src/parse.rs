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
    pub symbols: Vec<JavaSymbol>,
    /// Type-identifier occurrences (including declarations) for semantic refs.
    pub type_refs: Vec<JavaReference>,
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
            symbols: Vec::new(),
            type_refs: Vec::new(),
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
            push_type_ref(name, source, file);
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_type_refs(child, source, file);
    }
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
        "method_declaration" | "constructor_declaration" => {
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
    });

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

fn push_error(node: Node<'_>, file: &mut FileSymbols, message: &str) {
    let position = node.start_position();
    file.errors.push(ParseError {
        line: position.row + 1,
        column: position.column + 1,
        message: message.to_owned(),
    });
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
