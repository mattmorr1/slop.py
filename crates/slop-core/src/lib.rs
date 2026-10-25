//! slop's analyzer: per-function source facts (`parse`), the effect graph
//! (`graph`), SCIP symbol resolution (`resolve`) and repository-relative judgment (`analyze`).

pub mod analyze;
pub mod graph;
pub mod parse;
pub mod resolve;
