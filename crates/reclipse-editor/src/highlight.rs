//! A small, fast Java lexer for syntax highlighting.
//!
//! This deliberately does not try to be a parser. It is O(n), allocation-light
//! and produces byte spans that the UI turns into `egui::TextFormat` sections.
//! The tree-sitter parser remains the source of truth for symbols and errors.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Keyword,
    TypeName,
    String,
    Char,
    Comment,
    Number,
    Annotation,
    Literal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenSpan {
    pub start: usize,
    pub end: usize,
    pub kind: TokenKind,
}

/// Tokenize Java source. Whitespace and plain identifiers are omitted; the UI
/// paints gaps with the default text color.
pub fn tokenize_java(source: &str) -> Vec<TokenSpan> {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut spans = Vec::with_capacity(len / 24);
    let mut index = 0;

    while index < len {
        match bytes[index] {
            b'/' if index + 1 < len && bytes[index + 1] == b'/' => {
                let start = index;
                index += 2;
                while index < len && bytes[index] != b'\n' {
                    index += 1;
                }
                push(&mut spans, start, index, TokenKind::Comment);
            }
            b'/' if index + 1 < len && bytes[index + 1] == b'*' => {
                let start = index;
                index += 2;
                while index + 1 < len {
                    if bytes[index] == b'*' && bytes[index + 1] == b'/' {
                        index += 2;
                        break;
                    }
                    index += 1;
                }
                if index >= len {
                    index = len;
                }
                push(&mut spans, start, index, TokenKind::Comment);
            }
            b'"' => {
                let start = index;
                if index + 3 <= len && &bytes[index..index + 3] == b"\"\"\"" {
                    index += 3;
                    let mut closed = false;
                    while index + 3 <= len {
                        if &bytes[index..index + 3] == b"\"\"\"" {
                            index += 3;
                            closed = true;
                            break;
                        }
                        index += 1;
                    }
                    if !closed {
                        index = len;
                    }
                } else {
                    index += 1;
                    while index < len {
                        match bytes[index] {
                            b'\\' => index = (index + 2).min(len),
                            b'"' => {
                                index += 1;
                                break;
                            }
                            _ => index += 1,
                        }
                    }
                }
                push(&mut spans, start, index, TokenKind::String);
            }
            b'\'' => {
                let start = index;
                index += 1;
                while index < len {
                    match bytes[index] {
                        b'\\' => index = (index + 2).min(len),
                        b'\'' => {
                            index += 1;
                            break;
                        }
                        _ => index += 1,
                    }
                }
                push(&mut spans, start, index, TokenKind::Char);
            }
            b'@' if index + 1 < len && is_identifier_start(bytes[index + 1]) => {
                let start = index;
                index += 1;
                consume_identifier(bytes, &mut index);
                while index + 1 < len
                    && bytes[index] == b'.'
                    && is_identifier_start(bytes[index + 1])
                {
                    index += 1;
                    consume_identifier(bytes, &mut index);
                }
                push(&mut spans, start, index, TokenKind::Annotation);
            }
            b'0'..=b'9' => {
                let start = index;
                index += 1;
                while index < len {
                    let byte = bytes[index];
                    let exponent_sign = (byte == b'+' || byte == b'-')
                        && index > 0
                        && matches!(bytes[index - 1], b'e' | b'E');
                    if byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'_' || exponent_sign
                    {
                        index += 1;
                    } else {
                        break;
                    }
                }
                push(&mut spans, start, index, TokenKind::Number);
            }
            byte if is_identifier_start(byte) => {
                let start = index;
                consume_identifier(bytes, &mut index);
                let word = &source[start..index];
                if is_keyword(word) {
                    push(&mut spans, start, index, TokenKind::Keyword);
                } else if matches!(word, "true" | "false" | "null") {
                    push(&mut spans, start, index, TokenKind::Literal);
                } else if word.chars().next().is_some_and(char::is_uppercase) {
                    push(&mut spans, start, index, TokenKind::TypeName);
                }
            }
            _ => {
                index += source[index..]
                    .chars()
                    .next()
                    .map(char::len_utf8)
                    .unwrap_or(1);
            }
        }
    }

    spans
}

fn push(spans: &mut Vec<TokenSpan>, start: usize, end: usize, kind: TokenKind) {
    if end > start {
        spans.push(TokenSpan { start, end, kind });
    }
}

fn consume_identifier(bytes: &[u8], index: &mut usize) {
    while *index < bytes.len() && is_identifier_continue(bytes[*index]) {
        *index += 1;
    }
}

fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte == b'$' || byte >= 0x80
}

fn is_identifier_continue(byte: u8) -> bool {
    is_identifier_start(byte) || byte.is_ascii_digit()
}

fn is_keyword(word: &str) -> bool {
    matches!(
        word,
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
            | "exports"
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
            | "module"
            | "native"
            | "new"
            | "non-sealed"
            | "open"
            | "opens"
            | "package"
            | "permits"
            | "private"
            | "protected"
            | "provides"
            | "public"
            | "record"
            | "requires"
            | "return"
            | "sealed"
            | "short"
            | "static"
            | "strictfp"
            | "super"
            | "switch"
            | "synchronized"
            | "this"
            | "throw"
            | "throws"
            | "to"
            | "transient"
            | "transitive"
            | "try"
            | "uses"
            | "var"
            | "void"
            | "volatile"
            | "when"
            | "while"
            | "with"
            | "yield"
    )
}

/// Lexical state carried between lines by the incremental highlighter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HighlightState {
    #[default]
    Normal,
    BlockComment,
    TextBlock,
}

/// Tokenize one line with the state produced by the previous line.
///
/// Spans use byte offsets within `line`. Tokens that continue on the next line
/// (block comments and text blocks) update `state`.
pub fn tokenize_line(line: &str, state: &mut HighlightState) -> Vec<TokenSpan> {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut spans = Vec::with_capacity(len / 20);
    let mut index = 0;

    if *state == HighlightState::BlockComment {
        if let Some(end) = find_bytes(bytes, 0, b"*/") {
            push(&mut spans, 0, end + 2, TokenKind::Comment);
            *state = HighlightState::Normal;
            index = end + 2;
        } else {
            push(&mut spans, 0, len, TokenKind::Comment);
            return spans;
        }
    } else if *state == HighlightState::TextBlock {
        if let Some(end) = find_bytes(bytes, 0, b"\"\"\"") {
            push(&mut spans, 0, end + 3, TokenKind::String);
            *state = HighlightState::Normal;
            index = end + 3;
        } else {
            push(&mut spans, 0, len, TokenKind::String);
            return spans;
        }
    }

    while index < len {
        match bytes[index] {
            b'/' if index + 1 < len && bytes[index + 1] == b'/' => {
                push(&mut spans, index, len, TokenKind::Comment);
                break;
            }
            b'/' if index + 1 < len && bytes[index + 1] == b'*' => {
                if let Some(end) = find_bytes(bytes, index + 2, b"*/") {
                    push(&mut spans, index, end + 2, TokenKind::Comment);
                    index = end + 2;
                } else {
                    push(&mut spans, index, len, TokenKind::Comment);
                    *state = HighlightState::BlockComment;
                    break;
                }
            }
            b'"' if index + 3 <= len && &bytes[index..index + 3] == b"\"\"\"" => {
                if let Some(end) = find_bytes(bytes, index + 3, b"\"\"\"") {
                    push(&mut spans, index, end + 3, TokenKind::String);
                    index = end + 3;
                } else {
                    push(&mut spans, index, len, TokenKind::String);
                    *state = HighlightState::TextBlock;
                    break;
                }
            }
            b'"' => {
                let start = index;
                index += 1;
                while index < len {
                    match bytes[index] {
                        b'\\' => index = (index + 2).min(len),
                        b'"' => {
                            index += 1;
                            break;
                        }
                        _ => index += 1,
                    }
                }
                push(&mut spans, start, index, TokenKind::String);
            }
            b'\'' => {
                let start = index;
                index += 1;
                while index < len {
                    match bytes[index] {
                        b'\\' => index = (index + 2).min(len),
                        b'\'' => {
                            index += 1;
                            break;
                        }
                        _ => index += 1,
                    }
                }
                push(&mut spans, start, index, TokenKind::Char);
            }
            b'@' if index + 1 < len && is_identifier_start(bytes[index + 1]) => {
                let start = index;
                index += 1;
                consume_identifier(bytes, &mut index);
                while index + 1 < len
                    && bytes[index] == b'.'
                    && is_identifier_start(bytes[index + 1])
                {
                    index += 1;
                    consume_identifier(bytes, &mut index);
                }
                push(&mut spans, start, index, TokenKind::Annotation);
            }
            b'0'..=b'9' => {
                let start = index;
                index += 1;
                while index < len {
                    let byte = bytes[index];
                    let exponent_sign = (byte == b'+' || byte == b'-')
                        && index > 0
                        && matches!(bytes[index - 1], b'e' | b'E');
                    if byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'_' || exponent_sign
                    {
                        index += 1;
                    } else {
                        break;
                    }
                }
                push(&mut spans, start, index, TokenKind::Number);
            }
            byte if is_identifier_start(byte) => {
                let start = index;
                consume_identifier(bytes, &mut index);
                let word = &line[start..index];
                if is_keyword(word) {
                    push(&mut spans, start, index, TokenKind::Keyword);
                } else if matches!(word, "true" | "false" | "null") {
                    push(&mut spans, start, index, TokenKind::Literal);
                } else if word.chars().next().is_some_and(char::is_uppercase) {
                    push(&mut spans, start, index, TokenKind::TypeName);
                }
            }
            _ => {
                index += line[index..]
                    .chars()
                    .next()
                    .map(char::len_utf8)
                    .unwrap_or(1);
            }
        }
    }

    spans
}

fn find_bytes(haystack: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if from >= haystack.len() || needle.is_empty() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|window| from + window)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(source: &str) -> Vec<(TokenKind, String)> {
        tokenize_java(source)
            .into_iter()
            .map(|span| (span.kind, source[span.start..span.end].to_owned()))
            .collect()
    }

    #[test]
    fn highlights_keywords_types_strings_and_comments() {
        let source = "public class App { String s = \"hi\"; // note\n int x = 42; }";
        let tokens = kinds(source);
        assert!(tokens.contains(&(TokenKind::Keyword, "public".into())));
        assert!(tokens.contains(&(TokenKind::Keyword, "class".into())));
        assert!(tokens.contains(&(TokenKind::TypeName, "App".into())));
        assert!(tokens.contains(&(TokenKind::TypeName, "String".into())));
        assert!(tokens.contains(&(TokenKind::String, "\"hi\"".into())));
        assert!(tokens.contains(&(TokenKind::Comment, "// note".into())));
        assert!(tokens.contains(&(TokenKind::Number, "42".into())));
    }

    #[test]
    fn spans_are_sorted_and_non_overlapping() {
        let source = r#"
@SuppressWarnings("x")
class A {
    /* block */ char c = '\n';
    String t = """
       hello \"\"\"
       """;
}
"#;
        let spans = tokenize_java(source);
        assert!(!spans.is_empty());
        for pair in spans.windows(2) {
            assert!(pair[0].end <= pair[1].start, "{pair:?}");
        }
    }

    #[test]
    fn handles_unicode_identifiers() {
        let source = "class Café { String ação = \"olá\"; }";
        let tokens = kinds(source);
        assert!(tokens.contains(&(TokenKind::TypeName, "Café".into())));
        assert!(tokens.contains(&(TokenKind::String, "\"olá\"".into())));
    }

    #[test]
    fn line_lexer_carries_block_comments() {
        let mut state = HighlightState::Normal;
        let first = tokenize_line("class A { /* comment", &mut state);
        assert_eq!(state, HighlightState::BlockComment);
        assert_eq!(first.last().unwrap().kind, TokenKind::Comment);

        let second = tokenize_line("still comment */ int value = 1;", &mut state);
        assert_eq!(state, HighlightState::Normal);
        assert_eq!(second[0].kind, TokenKind::Comment);
        assert_eq!(
            &"still comment */ int value = 1;"[second[0].start..second[0].end],
            "still comment */"
        );
        assert!(second.iter().any(|token| token.kind == TokenKind::Keyword));
        assert!(second.iter().any(|token| token.kind == TokenKind::Number));
    }

    #[test]
    fn line_lexer_carries_text_blocks() {
        let mut state = HighlightState::Normal;
        let first = tokenize_line("String s = \"\"\"", &mut state);
        assert_eq!(state, HighlightState::TextBlock);
        assert_eq!(first.last().unwrap().kind, TokenKind::String);

        let middle = tokenize_line("hello \"world\"", &mut state);
        assert_eq!(state, HighlightState::TextBlock);
        assert_eq!(middle[0].kind, TokenKind::String);

        let last = tokenize_line("\"\"\"; class B {}", &mut state);
        assert_eq!(state, HighlightState::Normal);
        assert_eq!(&"\"\"\"; class B {}"[last[0].start..last[0].end], "\"\"\"");
        assert!(last.iter().any(|token| token.kind == TokenKind::Keyword));
    }
}
