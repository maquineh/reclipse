//! Reclipse Java engine: syntax indexing today, semantic analysis next.

pub mod index;
pub mod parse;
pub mod semantic;

pub use index::{build_index, IndexStats, IndexedProblem, ProjectIndex, SymbolLocation};
pub use parse::{
    parse_source, parse_source_with, FileSymbols, JavaReference, JavaSymbol, ParseError, SymbolKind,
};
