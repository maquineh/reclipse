//! Reclipse Java engine: syntax indexing today, semantic analysis next.

pub mod format;
pub mod index;
pub mod parse;
pub mod refactor;
pub mod semantic;

pub use index::{build_index, IndexStats, IndexedProblem, ProjectIndex, SymbolLocation};
pub use parse::{
    parse_local_bindings, parse_source, parse_source_with, FileSymbols, JavaMemberReference,
    JavaReference, JavaSymbol, JavaTestMethod, JavaTypeRelation, LocalBinding, LocalBindingKind,
    ParseError, SymbolKind, TestFramework,
};
