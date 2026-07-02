//! Name resolution behind an adapter (D5).
//!
//! The rest of `slop` depends only on [`Resolver`]: "given a reference site,
//! what definition does it point to?" The v1 backend ingests a `scip-python`
//! index as a batch artifact; a native `ty`/Ruff-backed resolver can be
//! swapped in later without touching the effect engine.

mod scip_backend;

pub use scip_backend::ScipResolver;

use serde::{Deserialize, Serialize};

/// Zero-based source range, end-exclusive (SCIP convention).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
}

impl Range {
    pub fn contains(&self, line: u32, col: u32) -> bool {
        let after_start =
            line > self.start_line || (line == self.start_line && col >= self.start_col);
        let before_end = line < self.end_line || (line == self.end_line && col < self.end_col);
        after_start && before_end
    }
}

/// What kind of entity a symbol names. scip-python leaves the protobuf
/// `kind` field unspecified, so this is derived from the SCIP symbol
/// grammar's descriptor suffix (`Name#` type, `name().` function, `name:`
/// namespace, `name.` term, `(name)` parameter).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SymbolKind {
    Module,
    Class,
    Function,
    Parameter,
    Term,
    TypeParameter,
    Macro,
    Local,
    Unknown,
}

impl SymbolKind {
    pub fn from_symbol(symbol: &str) -> Self {
        if symbol.starts_with("local ") {
            return SymbolKind::Local;
        }
        if symbol.ends_with("().") {
            SymbolKind::Function
        } else if symbol.ends_with('#') {
            SymbolKind::Class
        } else if symbol.ends_with(':') {
            SymbolKind::Module
        } else if symbol.ends_with(')') {
            SymbolKind::Parameter
        } else if symbol.ends_with(']') {
            SymbolKind::TypeParameter
        } else if symbol.ends_with('!') {
            SymbolKind::Macro
        } else if symbol.ends_with('.') {
            SymbolKind::Term
        } else {
            SymbolKind::Unknown
        }
    }
}

/// Where a symbol is defined.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Definition {
    /// Fully-qualified SCIP symbol, e.g.
    /// `scip-python python toy_repo 0.1 core/http_client/HttpClient#get().`
    pub symbol: String,
    /// Repo-relative path; `None` for external symbols (stdlib, site-packages)
    /// that have no definition inside the indexed repo. External resolution is
    /// load-bearing: the effect seed table keys off these symbols
    /// (`requests.get` -> Net).
    pub file: Option<String>,
    pub range: Option<Range>,
    /// Full span of the defined body (fn body, class body) when the indexer
    /// emitted it — the anchor for attributing references to their enclosing
    /// definition.
    pub enclosing_range: Option<Range>,
    pub kind: SymbolKind,
    /// Human-readable name, e.g. `HttpClient#get`.
    pub display_name: String,
    /// Documentation / signature text the indexer attached, when present.
    pub documentation: Vec<String>,
}

impl Definition {
    pub fn is_external(&self) -> bool {
        self.file.is_none()
    }
}

/// A single reference or definition occurrence inside a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Occurrence {
    pub symbol: String,
    pub range: Range,
    pub is_definition: bool,
    /// Body span, present on class/function definition occurrences.
    pub enclosing_range: Option<Range>,
}

/// The adapter interface (D5). Everything downstream — graph build, effect
/// inference, detectors — consumes only this.
pub trait Resolver {
    /// Resolve the reference at a position to its definition.
    fn resolve(&self, file: &str, line: u32, col: u32) -> Option<&Definition>;

    /// Resolve a SCIP symbol directly to its definition.
    fn definition_of(&self, symbol: &str) -> Option<&Definition>;

    /// All occurrences (definitions and references) recorded in a file,
    /// sorted by position.
    fn occurrences_in(&self, file: &str) -> &[Occurrence];

    /// Repo-relative paths of every indexed file.
    fn files(&self) -> Vec<&str>;
}
