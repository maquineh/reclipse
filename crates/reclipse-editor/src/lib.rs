//! Reclipse editor core: rope documents, search and a fast Java highlighter.

mod document;
mod highlight;
mod search;

pub use document::{
    char_index_for_line_col, identifier_at, line_col_at_char, slice_chars, CursorState, Document,
    EditKind,
};
pub use highlight::{tokenize_java, tokenize_line, HighlightState, TokenKind, TokenSpan};
pub use search::{find_all, find_all_with, SearchMatcher, SearchOptions};
