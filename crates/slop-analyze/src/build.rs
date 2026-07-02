//! Build the [`CodeGraph`] from a [`Resolver`]: one pass over every file's
//! occurrences producing Module/Class/Function nodes and Contains/Calls
//! edges. Effect edges are added afterwards by [`crate::effects`].

use std::collections::HashMap;

use petgraph::graph::NodeIndex;
use slop_graph::{CodeEntity, CodeGraph, EdgeKind, EffectSet, NodeType};
use slop_resolve::{Range, Resolver, SymbolKind};

use crate::entity_id::entity_id;

pub struct BuiltGraph {
    pub graph: CodeGraph,
    /// SCIP symbol -> node, for detectors that start from a symbol.
    pub by_symbol: HashMap<String, NodeIndex>,
}

pub fn build_graph(resolver: &dyn Resolver) -> BuiltGraph {
    let mut graph = CodeGraph::new();
    let mut by_symbol: HashMap<String, NodeIndex> = HashMap::new();

    // Pass 1: nodes. One Module node per file, one node per class/function
    // definition. Scopes (definition + body range) drive pass-2 attribution.
    struct Scope {
        node: NodeIndex,
        range: Range,
    }
    let mut scopes_by_file: HashMap<String, Vec<Scope>> = HashMap::new();
    let mut module_by_file: HashMap<String, NodeIndex> = HashMap::new();

    for file in resolver.files() {
        let mut scopes: Vec<Scope> = Vec::new();
        let mut module_node: Option<NodeIndex> = None;

        for occ in resolver.occurrences_in(file) {
            if !occ.is_definition {
                continue;
            }
            let kind = SymbolKind::from_symbol(&occ.symbol);
            let node_type = match kind {
                SymbolKind::Module => NodeType::Module,
                SymbolKind::Class => NodeType::Class,
                SymbolKind::Function => NodeType::Function,
                _ => continue,
            };
            let Some(id) = entity_id(&occ.symbol) else {
                continue;
            };
            let def = resolver.definition_of(&occ.symbol);
            // Prefer the body span: diff-mode intersects findings with
            // changed lines, and changes land in bodies, not name tokens.
            // A module's span is its whole file, so module-level findings
            // (circular imports) survive the diff filter on any file change.
            let span = if node_type == NodeType::Module {
                slop_resolve::Range {
                    start_line: 0,
                    start_col: 0,
                    end_line: u32::MAX,
                    end_col: 0,
                }
            } else {
                occ.enclosing_range.unwrap_or(occ.range)
            };
            let idx = graph.add_entity(CodeEntity {
                id,
                entity_type: node_type,
                name: def
                    .map(|d| d.display_name.clone())
                    .unwrap_or_else(|| occ.symbol.clone()),
                signature: String::new(),
                docstring: def.and_then(|d| d.documentation.first().cloned()),
                file: file.to_string(),
                source_range: (span.start_line as usize, span.end_line as usize),
                body_hash: String::new(),
                effect_signature: EffectSet::pure(),
            });
            by_symbol.insert(occ.symbol.clone(), idx);

            match node_type {
                NodeType::Module => module_node = Some(idx),
                _ => {
                    if let Some(body) = occ.enclosing_range {
                        scopes.push(Scope { node: idx, range: body });
                    }
                }
            }
        }

        if let Some(m) = module_node {
            module_by_file.insert(file.to_string(), m);
        }
        scopes_by_file.insert(file.to_string(), scopes);
    }

    // Pass 2: edges. Contains: innermost enclosing scope (or module) contains
    // each definition. Calls: each reference to a class/function/module from
    // inside a scope is a call/use edge from that scope's node.
    for file in resolver.files() {
        let scopes = &scopes_by_file[file];
        let module = module_by_file.get(file).copied();

        let enclosing = |line: u32, col: u32, exclude: Option<NodeIndex>| -> Option<NodeIndex> {
            scopes
                .iter()
                .filter(|s| Some(s.node) != exclude && s.range.contains(line, col))
                .min_by_key(|s| {
                    (
                        s.range.end_line - s.range.start_line,
                        s.range.end_col.wrapping_sub(s.range.start_col),
                    )
                })
                .map(|s| s.node)
                .or(module)
        };

        for occ in resolver.occurrences_in(file) {
            let kind = SymbolKind::from_symbol(&occ.symbol);
            if occ.is_definition {
                if !matches!(kind, SymbolKind::Class | SymbolKind::Function) {
                    continue;
                }
                let node = by_symbol[&occ.symbol];
                if let Some(parent) = enclosing(occ.range.start_line, occ.range.start_col, Some(node))
                {
                    graph.add_edge(parent, node, EdgeKind::Contains);
                }
            } else {
                // References: entities that can carry behavior. External
                // Terms count too — module-level effect sources like
                // `os.environ` are Terms; internal Terms (attributes) are
                // data access, not behavior.
                if !matches!(
                    kind,
                    SymbolKind::Class | SymbolKind::Function | SymbolKind::Module | SymbolKind::Term
                ) {
                    continue;
                }
                let Some(def) = resolver.definition_of(&occ.symbol) else {
                    continue;
                };
                if kind == SymbolKind::Term && !def.is_external() {
                    continue;
                }
                let from = match enclosing(occ.range.start_line, occ.range.start_col, None) {
                    Some(n) => n,
                    None => continue,
                };
                let to = match by_symbol.get(&occ.symbol) {
                    Some(&idx) => idx,
                    None => {
                        // External (stdlib / third-party): materialize once.
                        let Some(id) = entity_id(&occ.symbol) else {
                            continue;
                        };
                        let idx = graph.add_entity(CodeEntity {
                            id,
                            entity_type: NodeType::EffectSource,
                            name: def.display_name.clone(),
                            signature: String::new(),
                            docstring: None,
                            file: String::new(),
                            source_range: (0, 0),
                            body_hash: String::new(),
                            effect_signature: EffectSet::pure(),
                        });
                        by_symbol.insert(occ.symbol.clone(), idx);
                        idx
                    }
                };
                if from == to {
                    continue;
                }
                // Internal module reference = an import dependency between
                // files: Imports edge module->module (the SCC target).
                // Everything else is a Calls/use edge from the enclosing
                // scope, which carries effects.
                if kind == SymbolKind::Module && !def.is_external() {
                    if let Some(from_module) = module {
                        if from_module != to {
                            graph.add_edge(from_module, to, EdgeKind::Imports);
                        }
                        continue;
                    }
                }
                graph.add_edge(from, to, EdgeKind::Calls);
            }
        }
    }

    BuiltGraph { graph, by_symbol }
}
