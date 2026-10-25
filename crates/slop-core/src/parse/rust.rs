//! Rust per-function facts, via tree-sitter — the third-language
//! implementation behind [`crate::parse::Language`]. Mirrors the Python and JS
//! extractors' semantics so the parser-based detectors (duplication,
//! purity, branch coverage) behave the same across languages.
//!
//! Only `fn` items are entities. Closures are deliberately excluded: SCIP
//! doesn't emit them as symbols, so they'd never join to a graph node, and
//! they'd feed the duplication detectors pairs no one can act on.

use anyhow::{anyhow, Result};
use tree_sitter::{Node, Parser};

use crate::parse::ts_state::{branch_points, collect_bound, HashState, Tok};
use crate::parse::{FunctionFacts, MIN_SIGNIFICANT_TOKENS};

const FN_KINDS: &[&str] = &["function_item"];

/// Structural decision points (exclude boolean operators and `?`).
fn is_branch_stmt(kind: &str) -> bool {
    matches!(
        kind,
        "if_expression" | "match_arm" | "for_expression" | "while_expression" | "loop_expression"
    )
}


/// Node kinds that bind a name, paired with the field holding their pattern.
const BINDERS: &[(&str, &str)] =
    &[("parameter", "pattern"), ("let_declaration", "pattern"), ("for_expression", "pattern")];

/// Classify a leaf for the three hashes. Field and type names stay free: they
/// are named by their owner, not bound by this function.
fn classify<'a>(kind: &str, text: &'a str, bound: &std::collections::HashSet<String>) -> Tok<'a> {
    match kind {
        "identifier" if bound.contains(text) => Tok::Bound(text),
        "identifier" | "field_identifier" | "type_identifier" | "shorthand_field_identifier"
        | "primitive_type" => Tok::Free,
        "integer_literal" | "float_literal" | "string_content" | "char_literal"
        | "boolean_literal" => Tok::Literal,
        _ => Tok::Other,
    }
}

fn is_comment(kind: &str) -> bool {
    matches!(kind, "line_comment" | "block_comment" | "comment")
}

pub fn analyze_rust(source: &str) -> Result<Vec<FunctionFacts>> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .map_err(|e| anyhow!("loading tree-sitter grammar: {e}"))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow!("tree-sitter failed to parse"))?;
    if tree.root_node().has_error() {
        return Err(anyhow!("source does not parse as Rust"));
    }

    let src = source.as_bytes();
    let mut facts = Vec::new();
    collect(tree.root_node(), src, &mut facts);
    Ok(facts)
}

fn collect(node: Node, src: &[u8], facts: &mut Vec<FunctionFacts>) {
    if FN_KINDS.contains(&node.kind()) {
        if let Some(f) = function_facts(node, src) {
            facts.push(f);
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, src, facts);
    }
}

fn text(node: Node, src: &[u8]) -> Option<String> {
    node.utf8_text(src).ok().map(|s| s.to_string())
}

fn function_facts(node: Node, src: &[u8]) -> Option<FunctionFacts> {
    let name_node = node.child_by_field_name("name")?;
    let name = text(name_node, src)?;
    let body = node.child_by_field_name("body")?;

    // Start at the attribute/doc run so the span matches the SCIP
    // `enclosing_range` the graph join keys off.
    let run = leading_run(node, src);
    let start_line = run
        .last()
        .unwrap_or(&node)
        .start_position()
        .row as u32;
    let mut bound = std::collections::HashSet::new();
    collect_bound(node, src, BINDERS, FN_KINDS, &mut bound);
    let mut hasher = HashState::default();
    hash_walk(body, src, &bound, &mut hasher);
    let (body_hash, structural_hash, alpha_hash) = hasher.finish(MIN_SIGNIFICANT_TOKENS);


    FunctionFacts {
        name,
        name_line: name_node.start_position().row as u32,
        start_line,
        end_line: node.end_position().row as u32,
        branch_points: branch_points(body, is_branch_stmt, FN_KINDS),
        body_hash,
        structural_hash,
        alpha_hash,
        equiv_hash: String::new(),
        decorated: is_attributed(node, src),
    }
    .into()
}

/// Is `kind` a doc comment (`///`, `//!`, `/** */`)? Those belong to the item;
/// a plain `//` note above it does not.
fn is_doc_comment(node: Node, src: &[u8]) -> bool {
    is_comment(node.kind())
        && node
            .utf8_text(src)
            .is_ok_and(|t| t.starts_with("///") || t.starts_with("//!") || t.starts_with("/**"))
}

/// The contiguous run of attributes and doc comments directly above `node`,
/// innermost-last. This is the item as rust-analyzer sees it: its SCIP
/// `enclosing_range` starts at the run, not at the `fn` keyword, and the graph
/// join keys off that. Python's extractor includes decorators for the same
/// reason.
fn leading_run<'a>(node: Node<'a>, src: &[u8]) -> Vec<Node<'a>> {
    let mut run = Vec::new();
    let mut lowest = node.start_position().row;
    let mut sib = node.prev_sibling();
    while let Some(s) = sib {
        let attached = s.kind() == "attribute_item" || is_doc_comment(s, src);
        // A `line_comment` node spans rows N..N+1 (it swallows the newline)
        // while an `attribute_item` ends on its own row, so "directly above"
        // has to admit both.
        let adjacent = s.start_position().row < lowest && s.end_position().row + 1 >= lowest;
        if !attached || !adjacent {
            break;
        }
        lowest = s.start_position().row;
        run.push(s);
        sib = s.prev_sibling();
    }
    run
}

/// An attribute macro precedes the item. `#[test]`, `#[tokio::main]` and the
/// serde/clap derives are framework dispatch: the item is called without any
/// by-name reference, exactly like a Python decorator.
fn is_attributed(node: Node, src: &[u8]) -> bool {
    leading_run(node, src)
        .iter()
        .any(|n| n.kind() == "attribute_item")
}

fn hash_walk(node: Node, src: &[u8], bound: &std::collections::HashSet<String>, st: &mut HashState) {
    let kind = node.kind();
    if is_comment(kind) {
        return;
    }
    if node.child_count() == 0 {
        if kind.trim().is_empty() {
            return;
        }
        let text = node.utf8_text(src).unwrap_or("");
        st.leaf(kind, text, classify(kind, text, bound));
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if FN_KINDS.contains(&child.kind()) {
            continue;
        }
        hash_walk(child, src, bound, st);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn only(src: &str) -> FunctionFacts {
        let mut f = analyze_rust(src).unwrap();
        assert_eq!(f.len(), 1, "expected exactly one function");
        f.pop().unwrap()
    }

    #[test]
    fn extracts_functions_and_methods() {
        let src = "fn foo(a: i32, b: i32) -> i32 {\n    a + b\n}\nstruct S;\nimpl S {\n    fn bar(&self, x: i32) -> i32 { x }\n}\n";
        let facts = analyze_rust(src).unwrap();
        let names: Vec<&str> = facts.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["foo", "bar"]);
    }

    #[test]
    fn branch_points_count_structural_branches_only() {
        let src = "fn branchy(x: i32) -> i32 {\n    if x > 0 && x < 10 {\n        for i in 0..x { if i > 0 { return i; } }\n    } else if x < 0 || x == -5 {\n        while x > 0 { break; }\n    }\n    match x { 0 => 1, _ => 2 }\n}\n";
        // if, for, if, else-if, while, 2 arms = 7 (no booleans)
        assert_eq!(only(src).branch_points, 7);
    }

    #[test]
    fn structural_hash_ignores_names_exact_ignores_comments() {
        let a = "fn clean(rows: Vec<String>) -> Vec<String> {\n    let mut out = Vec::new();\n    for r in rows { if !r.is_empty() { out.push(r.trim().to_string()); } }\n    out\n}\n";
        let b = "fn scale(items: Vec<String>) -> Vec<String> {\n    // different comment\n    let mut acc = Vec::new();\n    for x in items { if !x.is_empty() { acc.push(x.trim().to_string()); } }\n    acc\n}\n";
        let fa = only(a);
        let fb = only(b);
        assert!(!fa.structural_hash.is_empty());
        assert_eq!(fa.structural_hash, fb.structural_hash);
        assert_ne!(fa.body_hash, fb.body_hash);
    }

    #[test]
    fn start_line_covers_the_doc_and_attribute_run() {
        // rust-analyzer's `enclosing_range` starts at the run, and the graph
        // join is keyed on it — if this drifts, half the Rust findings lose
        // their entity id and fall back to a path label.
        let src = "/// One.\n/// Two.\n#[inline]\npub fn thing(a: i32) -> i32 {\n    a\n}\n";
        assert_eq!(only(src).start_line, 0);
        // A plain comment is not part of the item.
        let plain = "// just a note\n\npub fn thing(a: i32) -> i32 {\n    a\n}\n";
        assert_eq!(only(plain).start_line, 2);
    }

    #[test]
    fn attributes_mark_the_item_as_framework_dispatched() {
        let src = "#[test]\nfn checks_a_thing() {\n    assert!(true);\n}\n";
        assert!(only(src).decorated);
        let plain = "fn checks_a_thing() {\n    assert!(true);\n}\n";
        assert!(!only(plain).decorated);
    }

    #[test]
    fn broken_source_is_an_error_not_silent_facts() {
        assert!(analyze_rust("fn oops( {\n").is_err());
    }
}


