//! Naming-convention detector (fuzzy => Advisory only, D12).
//!
//! Codebase-relative: the dominant case style is measured from the
//! function-name population, deviants are flagged. Plus absolute AI-slop
//! name patterns (`_v2`, `helper_`, `temp_`).

use slop_graph::NodeType;

use crate::build::BuiltGraph;
use crate::findings::{Finding, Severity};

const DOMINANCE: f64 = 0.9;
const MIN_POPULATION: usize = 5;

const SLOP_SUFFIXES: &[&str] = &["_v2", "_v3", "_new", "_old", "_final", "_copy", "_backup"];
const SLOP_PREFIXES: &[&str] = &["helper_", "temp_", "my_"];

#[derive(PartialEq, Clone, Copy)]
enum CaseStyle {
    Snake,
    Camel,
    Other,
}

fn case_style(name: &str) -> CaseStyle {
    let has_underscore = name.contains('_');
    let has_inner_upper = name.chars().skip(1).any(|c| c.is_uppercase());
    match (has_underscore, has_inner_upper) {
        (_, false) => CaseStyle::Snake, // single-word names count as snake
        (false, true) => CaseStyle::Camel,
        (true, true) => CaseStyle::Other,
    }
}

pub fn naming_convention(built: &BuiltGraph) -> Vec<Finding> {
    let functions: Vec<_> = built
        .graph
        .entities()
        .filter(|(_, e)| e.entity_type == NodeType::Function)
        .map(|(_, e)| e)
        .collect();

    let mut findings = Vec::new();

    // Population-relative case style.
    if functions.len() >= MIN_POPULATION {
        let named: Vec<(&str, CaseStyle)> = functions
            .iter()
            .map(|e| {
                let name = e.id.rsplit("::").next().unwrap_or(&e.id);
                (name, case_style(name))
            })
            .collect();
        // Whichever style dominates is the house style — a camelCase codebase
        // (JS/TS) is as much a convention as a snake_case one, and checking
        // only for snake made the rule silent on half the languages slop reads.
        let snake = named.iter().filter(|(_, s)| *s == CaseStyle::Snake).count();
        let camel = named.iter().filter(|(_, s)| *s == CaseStyle::Camel).count();
        let dominant = if snake >= camel { CaseStyle::Snake } else { CaseStyle::Camel };
        let (count, style_name) = match dominant {
            CaseStyle::Snake => (snake, "snake_case"),
            _ => (camel, "camelCase"),
        };
        if count as f64 / named.len() as f64 >= DOMINANCE {
            for (entity, (name, style)) in functions.iter().zip(&named) {
                if *style != dominant && !name.starts_with("__") {
                    findings.push(Finding {
                        rule: "naming-convention",
                        severity: Severity::Advisory,
                        entity: entity.id.clone(),
                        file: entity.file.clone(),
                        lines: entity.source_range,
                        related: Vec::new(),
                        message: format!(
                            "`{name}` deviates from this codebase's dominant {style_name} style ({count}/{} functions)",
                            named.len()
                        ),
                        fix_guidance: format!("Rename `{name}` to {style_name}"),
                    });
                }
            }
        }
    }

    // Absolute slop-name patterns. Normalize camelCase to snake_case first
    // so `fetchDataV2` matches `_v2`.
    for entity in &functions {
        let name = entity.id.rsplit("::").next().unwrap_or(&entity.id);
        let mut lower = String::with_capacity(name.len() + 4);
        for ch in name.chars() {
            if ch.is_uppercase() && !lower.is_empty() && !lower.ends_with('_') {
                lower.push('_');
            }
            lower.extend(ch.to_lowercase());
        }
        let suffix = SLOP_SUFFIXES.iter().find(|s| lower.ends_with(**s));
        let prefix = SLOP_PREFIXES.iter().find(|p| lower.starts_with(**p));
        if let Some(pattern) = suffix.or(prefix) {
            findings.push(Finding {
                rule: "slop-name",
                severity: Severity::Advisory,
                entity: entity.id.clone(),
                file: entity.file.clone(),
                lines: entity.source_range,
                related: Vec::new(),
                message: format!(
                    "`{name}` carries the throwaway marker `{pattern}` — a versioned/placeholder name that outlives its intent"
                ),
                fix_guidance: format!(
                    "Rename `{name}` to describe what it does; if it supersedes an older function, delete the old one instead of versioning the name"
                ),
            });
        }
    }

    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_style_classification() {
        assert!(matches!(case_style("parse_date"), CaseStyle::Snake));
        assert!(matches!(case_style("main"), CaseStyle::Snake));
        assert!(matches!(case_style("fetchData"), CaseStyle::Camel));
        assert!(matches!(case_style("fetch_Data"), CaseStyle::Other));
    }

    fn built_of(names: &[&str]) -> BuiltGraph {
        let mut graph = slop_graph::CodeGraph::new();
        for n in names {
            graph.add_entity(slop_graph::CodeEntity {
                id: format!("m::{n}"),
                entity_type: NodeType::Function,
                name: (*n).into(),
                signature: String::new(),
                docstring: None,
                file: "m.ts".into(),
                source_range: (0, 1),
                body_hash: String::new(),
                effect_signature: slop_graph::EffectSet::pure(),
            });
        }
        BuiltGraph {
            graph,
            by_symbol: Default::default(),
            referenced: Default::default(),
        }
    }

    #[test]
    fn a_camelcase_codebase_is_judged_against_camelcase() {
        let mut names: Vec<String> = (0..19).map(|i| format!("fetchThing{i}")).collect();
        names.push("parse_date".into());
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let found = naming_convention(&built_of(&refs));
        let convention: Vec<_> = found.iter().filter(|f| f.rule == "naming-convention").collect();
        assert_eq!(convention.len(), 1, "{convention:#?}");
        assert!(convention[0].message.contains("camelCase"), "{}", convention[0].message);
        assert!(convention[0].entity.ends_with("parse_date"));
    }
}
