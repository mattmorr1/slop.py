//! SCIP-backed [`Resolver`]: ingest an `index.scip` protobuf produced by
//! `scip-python` as a batch artifact. No live dependency on the indexer.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use protobuf::Message;

use crate::{Definition, Occurrence, Range, Resolver};

const SYMBOL_ROLE_DEFINITION: i32 = 0x1;

pub struct ScipResolver {
    definitions: HashMap<String, Definition>,
    occurrences_by_file: HashMap<String, Vec<Occurrence>>,
    empty: Vec<Occurrence>,
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
        let mut occurrences_by_file: HashMap<String, Vec<Occurrence>> = HashMap::new();

        // External symbols (stdlib, site-packages): definitions with no file.
        for info in &index.external_symbols {
            definitions
                .entry(info.symbol.clone())
                .or_insert_with(|| Definition {
                    symbol: info.symbol.clone(),
                    file: None,
                    range: None,
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
                if is_definition {
                    let info = doc_info.get(occ.symbol.as_str());
                    definitions.insert(
                        occ.symbol.clone(),
                        Definition {
                            symbol: occ.symbol.clone(),
                            file: Some(file.clone()),
                            range: Some(range),
                            display_name: display_name(
                                &occ.symbol,
                                info.map(|i| i.display_name.as_str()).unwrap_or(""),
                            ),
                            documentation: info
                                .map(|i| i.documentation.clone())
                                .unwrap_or_default(),
                        },
                    );
                }
                occs.push(Occurrence {
                    symbol: occ.symbol.clone(),
                    range,
                    is_definition,
                });
            }

            occs.sort_by_key(|o| (o.range.start_line, o.range.start_col));
            occurrences_by_file.insert(file, occs);
        }

        Self {
            definitions,
            occurrences_by_file,
            empty: Vec::new(),
        }
    }

    pub fn definition_count(&self) -> usize {
        self.definitions.len()
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
        self.definitions.get(&occ.symbol)
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
