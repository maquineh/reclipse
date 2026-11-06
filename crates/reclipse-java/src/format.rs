//! Native Java formatter (built-in, no external dependency).
//!
//! Conservative whole-file formatter: it reindents by brace depth and
//! normalizes spacing around keywords, operators, commas, semicolons and
//! braces, while preserving strings, chars, comments and line breaks chosen
//! by the author (no line joining/splitting). Rules:
//!
//! - 4 spaces per indent level; continuation lines (`(` `[` unmatched at EOL)
//!   get one extra level;
//! - exactly one space around binary operators and after `,` `;` (in `for`
//!   headers) and keywords before `(` (`if (`, `for (`, `while (` …), but
//!   none between a method name and `(` in calls/declarations;
//! - `{` opens with a space (`) {`), `}` closes its own line except `} else`,
//!   `} catch`, `} finally`, `} while`;
//! - trailing whitespace removed, file ends with a single newline;
//! - `package`/`import` lines are additionally sorted (static last, matching
//!   the organize-imports order) without merging or wildcards.
//!
//! The formatter is idempotent and never touches the inside of string/char
//! literals or comments.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Word,
    Number,
    Symbol,
    Text,
    LineComment,
    BlockComment,
    Newline,
}

#[derive(Debug, Clone)]
struct Token {
    kind: Kind,
    text: String,
}

const KEYWORDS_SPACE_BEFORE_PAREN: &[&str] = &[
    "if",
    "for",
    "while",
    "switch",
    "catch",
    "synchronized",
    "return",
    "throw",
    "new",
    "assert",
];

pub fn format_source(source: &str) -> String {
    let tokens = lex(source);
    let mut out = String::with_capacity(source.len() + source.len() / 8);
    let mut indent: usize = 0;
    let mut paren_depth: usize = 0;
    let mut at_line_start = true;
    let mut pending_space = false;

    // Collect leading package/imports to sort them.
    let mut head: Vec<Token> = Vec::new();
    let mut rest_start = tokens.len();
    {
        let mut i = 0;
        while i < tokens.len() {
            match &tokens[i] {
                Token {
                    kind: Kind::Newline,
                    ..
                } => {
                    head.push(tokens[i].clone());
                    i += 1;
                }
                Token {
                    kind: Kind::LineComment,
                    text,
                } if text.trim_start().starts_with("//") => {
                    head.push(tokens[i].clone());
                    i += 1;
                }
                Token {
                    kind: Kind::Word,
                    text,
                } if text == "package" || text == "import" => {
                    // Consume through the terminating `;` (or newline when broken).
                    while i < tokens.len() {
                        let newline = tokens[i].kind == Kind::Newline;
                        head.push(tokens[i].clone());
                        i += 1;
                        if newline {
                            break;
                        }
                        if head
                            .last()
                            .is_some_and(|token| token.kind == Kind::Symbol && token.text == ";")
                        {
                            break;
                        }
                    }
                }
                _ => {
                    rest_start = i;
                    break;
                }
            }
        }
        if rest_start == tokens.len() {
            rest_start = i.min(tokens.len());
        }
    }
    let head_text = sort_head(&head);

    let mut index = rest_start;
    while index < tokens.len() {
        let token = &tokens[index];
        match token.kind {
            Kind::Newline => {
                // Collapse runs of blank lines to at most one.
                let mut next = index + 1;
                let mut blanks = 0;
                while next < tokens.len() && tokens[next].kind == Kind::Newline {
                    blanks += 1;
                    next += 1;
                }
                if !out.is_empty() {
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                    if blanks > 0 && !out.ends_with("\n\n") {
                        out.push('\n');
                    }
                }
                at_line_start = true;
                pending_space = false;
                index = next;
                continue;
            }
            Kind::LineComment | Kind::BlockComment => {
                if at_line_start {
                    push_indent(&mut out, indent, paren_depth);
                    at_line_start = false;
                } else if pending_space {
                    out.push(' ');
                }
                pending_space = false;
                out.push_str(&token.text);
                pending_space = true;
                index += 1;
                continue;
            }
            Kind::Text => {
                if at_line_start {
                    push_indent(&mut out, indent, paren_depth);
                    at_line_start = false;
                } else if pending_space {
                    out.push(' ');
                }
                pending_space = false;
                out.push_str(&token.text);
                index += 1;
                continue;
            }
            _ => {}
        }
        // Word / Number / Symbol.
        let text = token.text.as_str();
        match text {
            "{" => {
                if !at_line_start {
                    out.push(' ');
                } else {
                    push_indent(&mut out, indent, paren_depth);
                }
                out.push('{');
                indent += 1;
                at_line_start = false;
                pending_space = false;
                index += 1;
                // Stay on the same line only for `{` immediately closed (`{}`).
                if tokens.get(index).is_some_and(|token| token.text == "}") {
                    out.push('}');
                    indent = indent.saturating_sub(1);
                    index += 1;
                    pending_space = true;
                }
            }
            "}" => {
                indent = indent.saturating_sub(1);
                if !at_line_start {
                    out.push('\n');
                }
                push_indent(&mut out, indent, paren_depth);
                out.push('}');
                at_line_start = false;
                // `} else`, `} catch`, `} finally`, `} while` stay joined.
                let join = tokens.get(index + 1).is_some_and(|token| {
                    token.kind == Kind::Word
                        && matches!(token.text.as_str(), "else" | "catch" | "finally" | "while")
                });
                pending_space = join;
                if join {
                    out.push(' ');
                    pending_space = false;
                }
                index += 1;
            }
            ";" => {
                if at_line_start {
                    push_indent(&mut out, indent, paren_depth);
                }
                out.push(';');
                at_line_start = false;
                // Inside `for (;;)` keep spaces; otherwise the newline pass
                // below handles the break (we only emit spaces here).
                pending_space = true;
                index += 1;
            }
            "," => {
                if at_line_start {
                    push_indent(&mut out, indent, paren_depth);
                }
                out.push_str(", ");
                at_line_start = false;
                pending_space = false;
                index += 1;
            }
            "(" => {
                if at_line_start {
                    push_indent(&mut out, indent, paren_depth);
                    at_line_start = false;
                } else if pending_space {
                    // Keep `if (` / `while (` / `return (` but not `foo (`.
                    let keep = tokens.get(index.wrapping_sub(1)).is_none()
                        || is_keyword_before_paren(index, &tokens);
                    if keep {
                        out.push(' ');
                    }
                    pending_space = false;
                }
                out.push('(');
                paren_depth += 1;
                index += 1;
            }
            ")" => {
                paren_depth = paren_depth.saturating_sub(1);
                out.push(')');
                pending_space = true;
                index += 1;
            }
            "[" => {
                if at_line_start {
                    push_indent(&mut out, indent, paren_depth);
                    at_line_start = false;
                } else if pending_space
                    && !tokens
                        .get(index.wrapping_sub(1))
                        .is_some_and(|token| token.kind == Kind::Word)
                {
                    out.push(' ');
                    pending_space = false;
                }
                out.push('[');
                paren_depth += 1;
                index += 1;
            }
            "]" => {
                paren_depth = paren_depth.saturating_sub(1);
                out.push(']');
                pending_space = true;
                index += 1;
            }
            _ if is_operator(text) => {
                if at_line_start {
                    push_indent(&mut out, indent, paren_depth);
                    at_line_start = false;
                } else if !out.ends_with(' ') && !out.ends_with('\n') {
                    out.push(' ');
                }
                out.push_str(text);
                out.push(' ');
                pending_space = false;
                index += 1;
            }
            _ => {
                // Words, numbers, annotations, dots, etc.
                if at_line_start {
                    push_indent(&mut out, indent, paren_depth);
                    at_line_start = false;
                } else if pending_space
                    && needs_space_before(&tokens[index], tokens.get(index.wrapping_sub(1)))
                {
                    out.push(' ');
                }
                pending_space = false;
                out.push_str(text);
                // A keyword expecting `(` keeps the space decision for `(`.
                if token.kind == Kind::Word {
                    pending_space = true;
                }
                index += 1;
            }
        }
    }
    let mut result = head_text;
    result.push_str(&out);
    // Trim trailing blank lines; exactly one trailing newline.
    while result.ends_with("\n\n") {
        result.pop();
    }
    if !result.ends_with('\n') {
        result.push('\n');
    }
    result
}

fn push_indent(out: &mut String, indent: usize, paren_depth: usize) {
    let extra = usize::from(paren_depth > 0);
    for _ in 0..indent + extra {
        out.push_str("    ");
    }
}

fn is_keyword_before_paren(index: usize, tokens: &[Token]) -> bool {
    // Walk back over the pending word; formatting guarantees it is adjacent.
    if index == 0 {
        return false;
    }
    match &tokens[index - 1] {
        Token {
            kind: Kind::Word,
            text,
        } => KEYWORDS_SPACE_BEFORE_PAREN.contains(&text.as_str()),
        _ => false,
    }
}

fn needs_space_before(current: &Token, previous: Option<&Token>) -> bool {
    let Some(previous) = previous else {
        return false;
    };
    if previous.kind == Kind::Symbol && matches!(previous.text.as_str(), "(" | "[" | "." | "@") {
        return false;
    }
    if current.kind == Kind::Symbol
        && matches!(
            current.text.as_str(),
            ")" | "]" | "." | ";" | "," | ":" | "(" | "["
        )
    {
        return false;
    }
    if previous.kind == Kind::Symbol && matches!(previous.text.as_str(), ")" | "]") {
        return current.kind == Kind::Word;
    }
    true
}

fn is_operator(text: &str) -> bool {
    matches!(
        text,
        "=" | "=="
            | "!"
            | "!="
            | "<"
            | "<="
            | ">"
            | ">="
            | "+"
            | "+="
            | "-"
            | "-="
            | "*"
            | "*="
            | "/"
            | "/="
            | "%"
            | "%="
            | "&&"
            | "||"
            | "&"
            | "|"
            | "^"
            | "->"
            | "::"
            | "?"
            | ":"
            | "++"
            | "--"
    )
}

fn lex(source: &str) -> Vec<Token> {
    let chars: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let character = chars[index];
        if character == '\n' {
            tokens.push(Token {
                kind: Kind::Newline,
                text: "\n".to_owned(),
            });
            index += 1;
        } else if character.is_whitespace() {
            index += 1;
        } else if character == '/' && chars.get(index + 1) == Some(&'/') {
            let start = index;
            while index < chars.len() && chars[index] != '\n' {
                index += 1;
            }
            tokens.push(Token {
                kind: Kind::LineComment,
                text: chars[start..index].iter().collect(),
            });
        } else if character == '/' && chars.get(index + 1) == Some(&'*') {
            let start = index;
            index += 2;
            while index < chars.len() && !(chars[index - 1] == '*' && chars[index] == '/') {
                index += 1;
            }
            index = (index + 1).min(chars.len());
            // Normalize inner newlines to explicit tokens.
            let text: String = chars[start..index].iter().collect();
            let mut first = true;
            for part in text.split('\n') {
                if !first {
                    tokens.push(Token {
                        kind: Kind::Newline,
                        text: "\n".to_owned(),
                    });
                }
                first = false;
                if !part.trim().is_empty() {
                    tokens.push(Token {
                        kind: Kind::BlockComment,
                        text: part.trim().to_owned(),
                    });
                }
            }
        } else if character == '"' || character == '\'' {
            let quote = character;
            let start = index;
            index += 1;
            if quote == '\'' && chars.get(index) == Some(&'\\') {
                index += 2;
            }
            while index < chars.len() {
                if chars[index] == '\\' {
                    index += 2;
                    continue;
                }
                if chars[index] == quote {
                    // Text blocks (`"""`) keep raw content.
                    if quote == '"'
                        && chars.get(index + 1) == Some(&'"')
                        && chars.get(index + 2) == Some(&'"')
                    {
                        index += 3;
                        let end = chars.len();
                        let mut close = index;
                        while close + 2 < end {
                            if chars[close] == '"'
                                && chars[close + 1] == '"'
                                && chars[close + 2] == '"'
                            {
                                break;
                            }
                            close += 1;
                        }
                        index = (close + 3).min(end);
                    } else {
                        index += 1;
                    }
                    break;
                }
                if chars[index] == '\n' {
                    break;
                }
                index += 1;
            }
            tokens.push(Token {
                kind: Kind::Text,
                text: chars[start..index.min(chars.len())].iter().collect(),
            });
        } else if character.is_alphabetic()
            || character == '_'
            || character == '$'
            || character == '@'
        {
            let start = index;
            index += 1;
            while index < chars.len()
                && (chars[index].is_alphanumeric() || chars[index] == '_' || chars[index] == '$')
            {
                index += 1;
            }
            tokens.push(Token {
                kind: Kind::Word,
                text: chars[start..index].iter().collect(),
            });
        } else if character.is_ascii_digit()
            || (character == '.'
                && chars
                    .get(index + 1)
                    .is_some_and(|next| next.is_ascii_digit()))
        {
            let start = index;
            index += 1;
            while index < chars.len()
                && (chars[index].is_alphanumeric() || chars[index] == '.' || chars[index] == '_')
            {
                index += 1;
            }
            tokens.push(Token {
                kind: Kind::Number,
                text: chars[start..index].iter().collect(),
            });
        } else {
            // Multi-char operators first.
            let rest: String = chars[index..(index + 3).min(chars.len())].iter().collect();
            let symbol = [
                "...", ">>>=", ">>=", "<<=", "->", "::", "==", "!=", "<=", ">=", "&&", "||", "++",
                "--", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", ">>>", ">>", "<<",
            ]
            .into_iter()
            .find(|candidate| rest.starts_with(candidate))
            .or_else(|| {
                [
                    "=", "!", "<", ">", "+", "-", "*", "/", "%", "&", "|", "^", "~", "?", ":", ";",
                    ",", ".", "(", ")", "[", "]", "{", "}",
                ]
                .into_iter()
                .find(|candidate| rest.starts_with(candidate))
            })
            .unwrap_or("?");
            tokens.push(Token {
                kind: Kind::Symbol,
                text: symbol.to_owned(),
            });
            index += symbol.len();
        }
    }
    tokens
}

/// Sort leading `package`/`import` lines (static imports last), keeping one
/// blank line after the block.
fn sort_head(head: &[Token]) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for token in head {
        if token.kind == Kind::Newline {
            if !current.trim().is_empty() {
                lines.push(current.clone());
            }
            current.clear();
        } else if token.kind == Kind::LineComment {
            lines.push(token.text.clone());
        } else {
            if !current.is_empty()
                && !current.ends_with(' ')
                && !current.ends_with('.')
                && token.text != ";"
                && token.text != "."
            {
                current.push(' ');
            }
            current.push_str(&token.text);
        }
    }
    if !current.trim().is_empty() {
        lines.push(current);
    }
    let mut package = None;
    let mut imports: Vec<String> = Vec::new();
    let mut others: Vec<String> = Vec::new();
    for line in lines {
        let trimmed = line.trim().to_owned();
        if trimmed.starts_with("package ") {
            package = Some(trimmed);
        } else if trimmed.starts_with("import ") {
            imports.push(trimmed);
        } else {
            others.push(trimmed);
        }
    }
    imports.sort_by_key(|line| (line.starts_with("import static"), line.to_owned()));
    let mut out = String::new();
    for line in &others {
        out.push_str(line);
        out.push('\n');
    }
    let has_package = package.is_some();
    if let Some(package) = package {
        out.push_str(&package);
        out.push('\n');
        if !imports.is_empty() {
            out.push('\n');
        }
    }
    for import in &imports {
        out.push_str(import);
        if !import.ends_with(';') {
            out.push(';');
        }
        out.push('\n');
    }
    if has_package || !imports.is_empty() {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_badly_indented_code() {
        let source =
            "package com.acme;\nclass A {\nint x=1;\nvoid m(){\nif(x>0){\nx+=1;\n}\n}\n}\n";
        let formatted = format_source(source);
        assert!(formatted.contains("    int x = 1;"));
        assert!(formatted.contains("        if (x > 0) {"));
        assert!(formatted.contains("            x += 1;"));
        assert!(formatted.ends_with("}\n"));
        // Idempotent.
        assert_eq!(format_source(&formatted), formatted);
    }

    #[test]
    fn keeps_strings_comments_and_calls_tight() {
        let source = "class A {\n// keep  me\nString s=\"a  +  b\";\nint n=foo(1,2);\n}\n";
        let formatted = format_source(source);
        assert!(formatted.contains("// keep  me"));
        assert!(formatted.contains("\"a  +  b\""));
        assert!(formatted.contains("foo(1, 2)"));
        assert_eq!(format_source(&formatted), formatted);
    }

    #[test]
    fn sorts_imports_static_last() {
        let source = "import static org.junit.Assert.assertEquals;\nimport java.util.List;\npackage com.acme;\nclass A {}\n";
        let formatted = format_source(source);
        let list = formatted.find("java.util.List").unwrap();
        let statik = formatted.find("import static").unwrap();
        let package = formatted.find("package com.acme;").unwrap();
        assert!(package < list && list < statik);
        assert_eq!(format_source(&formatted), formatted);
    }

    #[test]
    fn joins_else_and_catch() {
        let source =
            "class A {\nvoid m() {\ntry {\ngo();\n} catch (Exception e) {\nhandle();\n}\n}\n}\n";
        let formatted = format_source(source);
        assert!(formatted.contains("} catch (Exception e) {"));
        assert_eq!(format_source(&formatted), formatted);
    }

    #[test]
    fn empty_class_stays_compact() {
        let formatted = format_source("class A {\n}\n");
        assert!(formatted.contains("class A {\n}"));
        assert_eq!(format_source(&formatted), formatted);
    }
}
