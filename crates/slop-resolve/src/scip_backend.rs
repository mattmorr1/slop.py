//! SCIP-backed [`Resolver`]: ingest an `index.scip` protobuf produced by
//! `scip-python` as a batch artifact. No live dependency on the indexer.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use protobuf::Message;

use crate::{Definition, Occurrence, Range, Resolver, SymbolKind};

const SYMBOL_ROLE_DEFINITION: i32 = 0x1;

pub struct ScipResolver {
    definitions: HashMap<String, Definition>,
    /// SCIP `local N` symbols are document-scoped, so they live in a
    /// per-file map — a global one would collide across files.
    local_definitions: HashMap<(String, String), Definition>,
    occurrences_by_file: HashMap<String, Vec<Occurrence>>,
    empty: Vec<Occurrence>,
}

fn is_local(symbol: &str) -> bool {
    symbol.starts_with("local ")
}

impl ScipResolver {
    pub fn load(index_path: &Path) -> Result<Self> {
        let bytes = std::fs::read(index_path)
            .with_context(|| format!("reading SCIP index at {}", index_path.display()))?;
        let index = scip::types::Index::parse_from_bytes(&bytes)
            .context("parsing SCIP index protobuf")?;
        Ok(Self::from_index(index))
    }

    fn from_index(index: scip::types::Index) -> Self {
        let mut definitions: HashMap<String, Definition> = HashMap::new();
        let mut local_definitions: HashMap<(String, String), Definition> = HashMap::new();
        let mut occurrences_by_file: HashMap<String, Vec<Occurrence>> = HashMap::new();

        // External symbols (stdlib, site-packages): definitions with no file.
        for info in &index.external_symbols {
            definitions
                .entry(info.symbol.clone())
                .or_insert_with(|| Definition {
                    symbol: info.symbol.clone(),
                    file: None,
                    range: None,
                    enclosing_range: None,
                    kind: SymbolKind::from_symbol(&info.symbol),
                    display_name: display_name(&info.symbol, &info.display_name),
                    documentation: info.documentation.clone(),
                });
        }

        for doc in &index.documents {
            let file = doc.relative_path.clone();
            let mut occs: Vec<Occurrence> = Vec::with_capacity(doc.occurrences.len());

            // Symbol metadata declared in this document (docstrings, names).
            let mut doc_info: HashMap<&str, &scip::types::SymbolInformation> = HashMap::new();
            for info in &doc.symbols {
                doc_info.insert(info.symbol.as_str(), info);
            }

            for occ in &doc.occurrences {
                let Some(range) = parse_range(&occ.range) else {
                    continue;
                };
                let is_definition = occ.symbol_roles & SYMBOL_ROLE_DEFINITION != 0;
                let enclosing_range = parse_range(&occ.enclosing_range);
                if is_definition {
                    let info = doc_info.get(occ.symbol.as_str());
                    let def = Definition {
                        symbol: occ.symbol.clone(),
                        file: Some(file.clone()),
                        range: Some(range),
                        enclosing_range,
                        kind: SymbolKind::from_symbol(&occ.symbol),
                        display_name: display_name(
                            &occ.symbol,
                            info.map(|i| i.display_name.as_str()).unwrap_or(""),
                        ),
                        documentation: info
                            .map(|i| i.documentation.clone())
                            .unwrap_or_default(),
                    };
                    if is_local(&occ.symbol) {
                        local_definitions.insert((file.clone(), occ.symbol.clone()), def);
                    } else {
                        definitions.insert(occ.symbol.clone(), def);
                    }
                }
                occs.push(Occurrence {
                    symbol: occ.symbol.clone(),
                    range,
                    is_definition,
                    enclosing_range,
                });
            }

            occs.sort_by_key(|o| (o.range.start_line, o.range.start_col));
            occurrences_by_file.insert(file, occs);
        }

        // scip-python references stdlib/module symbols (e.g.
        // `python-stdlib 3.11 'urllib.request'/__init__:`) without listing
        // them in external_symbols. Synthesize file-less definitions so
        // references to them still resolve — the effect seed table keys off
        // exactly these symbols.
        for occs in occurrences_by_file.values() {
            for occ in occs {
                if !is_local(&occ.symbol) && !definitions.contains_key(&occ.symbol) {
                    definitions.insert(
                        occ.symbol.clone(),
                        Definition {
                            symbol: occ.symbol.clone(),
                            file: None,
                            range: None,
                            enclosing_range: None,
                            kind: SymbolKind::from_symbol(&occ.symbol),
                            display_name: display_name(&occ.symbol, ""),
                            documentation: Vec::new(),
                        },
                    );
                }
            }
        }

        Self {
            definitions,
            local_definitions,
            occurrences_by_file,
            empty: Vec::new(),
        }
    }

    pub fn definition_count(&self) -> usize {
        self.definitions.len()
    }

    pub fn document_count(&self) -> usize {
        self.occurrences_by_file.len()
    }

    /// Definition of a document-scoped `local N` symbol within `file`.
    pub fn local_definition_of(&self, file: &str, symbol: &str) -> Option<&Definition> {
        self.local_definitions
            .get(&(file.to_string(), symbol.to_string()))
    }
}

impl Resolver for ScipResolver {
    fn resolve(&self, file: &str, line: u32, col: u32) -> Option<&Definition> {
        let occs = self.occurrences_by_file.get(file)?;
        // Innermost (smallest) occurrence covering the position wins; SCIP
        // can nest e.g. an attribute occurrence inside an expression's.
        let occ = occs
            .iter()
            .filter(|o| o.range.contains(line, col))
            .min_by_key(|o| {
                (o.range.end_line - o.range.start_line, o.range.end_col.wrapping_sub(o.range.start_col))
            })?;
        if is_local(&occ.symbol) {
            self.local_definitions
                .get(&(file.to_string(), occ.symbol.clone()))
        } else {
            self.definitions.get(&occ.symbol)
        }
    }

    fn definition_of(&self, symbol: &str) -> Option<&Definition> {
        self.definitions.get(symbol)
    }

    fn occurrences_in(&self, file: &str) -> &[Occurrence] {
        self.occurrences_by_file
            .get(file)
            .unwrap_or(&self.empty)
    }

    fn files(&self) -> Vec<&str> {
        let mut files: Vec<&str> = self.occurrences_by_file.keys().map(|s| s.as_str()).collect();
        files.sort();
        files
    }
}

/// SCIP encodes ranges as `[startLine, startCol, endLine, endCol]`, or
/// `[line, startCol, endCol]` when the occurrence is single-line.
fn parse_range(raw: &[i32]) -> Option<Range> {
    match raw {
        [line, start_col, end_col] => Some(Range {
            start_line: *line as u32,
            start_col: *start_col as u32,
            end_line: *line as u32,
            end_col: *end_col as u32,
        }),
        [start_line, start_col, end_line, end_col] => Some(Range {
            start_line: *start_line as u32,
            start_col: *start_col as u32,
            end_line: *end_line as u32,
            end_col: *end_col as u32,
        }),
        _ => None,
    }
}

/// Fall back to the last descriptor of the SCIP symbol when the indexer
/// didn't attach a display name.
fn display_name(symbol: &str, provided: &str) -> String {
    if !provided.is_empty() {
        return provided.to_string();
    }
    symbol
        .rsplit('/')
        .next()
        .unwrap_or(symbol)
        .trim_end_matches('.')
        .to_string()
}
