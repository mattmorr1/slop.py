//! Reparse overlay: a document edited since indexing gets fresh entity line
//! ranges from the parser, without waiting for a reindex.
//!
//! Functions map to their old entities by (name, ordinal among same-named
//! functions), the mapping `calibrate` uses on history. Every other entity in
//! the document (classes, constants) shifts by the delta of the nearest mapped
//! function before it. Edges stay those of the last index, which is why such a
//! document is `Reparsed`, not `Resolved`: context may use it, a deny may not.

use std::collections::{BTreeMap, HashMap, HashSet};

use petgraph::graph::NodeIndex;
use slop_graph::NodeType;

use crate::build::BuiltGraph;
use crate::snapshot::DocumentState;
use crate::source::FileFacts;

/// Remap every stale document that facts can anchor; returns entities whose
/// function no longer exists. A document with no anchor stays `Stale`.
pub fn remap(built: &mut BuiltGraph, facts: &[FileFacts], degraded: &mut BTreeMap<String, DocumentState>) -> HashSet<NodeIndex> {
    let mut by_file: HashMap<String, Vec<NodeIndex>> = HashMap::new();
    for (idx, entity) in built.graph.entities() {
        if degraded.get(&entity.file) == Some(&DocumentState::Stale) {
            by_file.entry(entity.file.clone()).or_default().push(idx);
        }
    }
    let mut gone = HashSet::new();
    for file_facts in facts {
        let Some(entities) = by_file.get(file_facts.file.as_str()) else { continue };
        let graph = &mut built.graph.graph;
        let mut old: HashMap<String, Vec<NodeIndex>> = HashMap::new();
        entities.iter().filter(|&&idx| graph[idx].entity_type == NodeType::Function).for_each(|&idx| {
            old.entry(graph[idx].name.clone()).or_default().push(idx);
        });
        let mut new: HashMap<&str, Vec<(usize, usize)>> = HashMap::new();
        for fact in &file_facts.functions {
            new.entry(fact.name.as_str()).or_default().push((fact.start_line as usize, fact.end_line as usize));
        }
        // Anchors are (old start, new start); assignments give mapped functions their new range.
        let (mut anchors, mut assigned, mut missing) = (Vec::new(), HashMap::new(), HashSet::new());
        for (name, list) in &mut old {
            list.sort_by_key(|&idx| graph[idx].source_range.0);
            match new.get_mut(name.as_str()) {
                None => missing.extend(list.iter().copied()),
                Some(ranges) if ranges.len() == list.len() => {
                    ranges.sort_unstable();
                    for (&idx, &range) in list.iter().zip(ranges.iter()) {
                        anchors.push((graph[idx].source_range.0, range.0));
                        assigned.insert(idx, range);
                    }
                }
                Some(_) => {}
            }
        }
        if anchors.is_empty() {
            continue;
        }
        anchors.sort_unstable();
        for &idx in entities {
            if missing.contains(&idx) {
                gone.insert(idx);
            } else if let Some(&range) = assigned.get(&idx) {
                graph[idx].source_range = range;
            } else {
                let (start, end) = graph[idx].source_range;
                // ponytail: one delta per entity; an edit inside a class before its first method
                // shifts the class header by the previous anchor's delta until the reindex lands.
                let &(old, new) = anchors.iter().rev().find(|(old, _)| *old <= start).unwrap_or(&anchors[0]);
                let delta = new as isize - old as isize;
                graph[idx].source_range = (start.saturating_add_signed(delta), end.saturating_add_signed(delta));
            }
        }
        degraded.insert(file_facts.file.clone(), DocumentState::Reparsed);
    }
    gone
}
