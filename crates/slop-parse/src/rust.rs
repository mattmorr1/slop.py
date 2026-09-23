//! Rust per-function facts, via tree-sitter — the third-language
//! implementation behind [`crate::Language`]. Mirrors the Python and JS
//! extractors' semantics so the parser-based detectors (duplication,
//! complexity, over-commenting) behave the same across languages.
//!
//! Only `fn` items are entities. Closures are deliberately excluded: SCIP
//! doesn't emit them as symbols, so they'd never join to a graph node, and
//! they'd feed the duplication detectors pairs no one can act on.

use anyhow::{anyhow, Result};
use tree_sitter::{Node, Parser};

use crate::ts_state::{collect_bound, stmt_spans, ControlFlow, HashState, Tok};
use crate::{FunctionFacts, MIN_SIGNIFICANT_TOKENS};

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
    let signature = String::from_utf8_lossy(&src[node.start_byte()..body.start_byte()])
        .trim_end()
        .to_string();

    let mut bound = std::collections::HashSet::new();
    collect_bound(node, src, BINDERS, FN_KINDS, &mut bound);
    let mut hasher = HashState::default();
    hash_walk(body, src, &bound, &mut hasher);
    let significant_tokens = hasher.significant;
    let comment_lines = hasher.comment_lines;
    let code_lines = hasher.code_lines.len() as u32;
    let (body_hash, structural_hash, alpha_hash) = hasher.finish(MIN_SIGNIFICANT_TOKENS);

    let mut cf = ControlFlow::default();
    cf_walk(body, 1, &mut cf);

    let params = node.child_by_field_name("parameters");
    let param_count = params.map(|p| p.named_child_count() as u32).unwrap_or(0);
    let (forward_target, forward_identity) = forwarding(body, params, src);

    FunctionFacts {
        name,
        name_line: name_node.start_position().row as u32,
        start_line,
        end_line: node.end_position().row as u32,
        stmt_spans: stmt_spans(body),
        signature,
        complexity: cf.complexity,
        branch_points: cf.branch_points,
        max_nesting_depth: cf.max_depth,
        deepest_line: cf.deepest_line,
        body_hash,
        structural_hash,
        alpha_hash,
        significant_tokens,
        comment_lines,
        code_lines,
        decorated: is_attributed(node, src),
        param_count,
        // Rust returns the tail expression implicitly, so a declared return
        // type is the signal — an explicit `return` is the exception here.
        returns_value: node.child_by_field_name("return_type").is_some() || returns_value(body),
        forward_target,
        forward_identity,
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
        st.comment_lines += node.end_position().row as u32 - node.start_position().row as u32 + 1;
        return;
    }
    if node.child_count() == 0 {
        if kind.trim().is_empty() {
            return;
        }
        let text = node.utf8_text(src).unwrap_or("");
        st.leaf(kind, text, node.start_position().row as u32, classify(kind, text, bound));
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

/// Extra execution paths `node` itself introduces without being a structural
/// branch: `?`, and the short-circuit operators. The operator is an anonymous
/// child in this grammar, not an `operator` field as in the JS one. Direct
/// children only, so a nested `&&` is counted once, by its own node.
fn path_ops(node: Node) -> u32 {
    match node.kind() {
        "try_expression" => 1,
        "binary_expression" => {
            let mut c = node.walk();
            let short_circuit = node.children(&mut c).any(|n| matches!(n.kind(), "&&" | "||"));
            u32::from(short_circuit)
        }
        _ => 0,
    }
}

/// `cf_walk` for a node handed to us directly rather than reached as a child —
/// an `if` condition is the whole expression, so its own operators would
/// otherwise go uncounted.
fn cf_walk_from(node: Node, depth: u32, cf: &mut ControlFlow) {
    cf.complexity += path_ops(node);
    cf_walk(node, depth, cf);
}

/// Count cyclomatic complexity, structural branch points, and nesting depth
/// over a subtree, skipping nested functions. `else if` is kept flat, matching
/// the Python and JS extractors.
fn cf_walk(node: Node, depth: u32, cf: &mut ControlFlow) {
    if cf.complexity == 0 {
        cf.complexity = 1;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let kind = child.kind();
        if FN_KINDS.contains(&kind) {
            continue;
        }

        cf.complexity += path_ops(child);

        if is_branch_stmt(kind) {
            cf.complexity += 1;
            cf.branch_points += 1;
            cf.reached(depth, child.start_position().row as u32);

            if kind == "if_expression" {
                if let Some(cond) = child.child_by_field_name("condition") {
                    cf_walk_from(cond, depth, cf);
                }
                if let Some(cons) = child.child_by_field_name("consequence") {
                    cf_walk(cons, depth + 1, cf);
                }
                if let Some(alt) = child.child_by_field_name("alternative") {
                    descend_else(alt, depth, cf);
                }
                continue;
            }
            cf_walk(child, depth + 1, cf);
            continue;
        }

        cf_walk(child, depth, cf);
    }
}

/// The `else` arm: an `else if` chain stays at `depth`; a plain `else` block
/// goes one level deeper.
fn descend_else(else_clause: Node, depth: u32, cf: &mut ControlFlow) {
    let mut cursor = else_clause.walk();
    for child in else_clause.children(&mut cursor) {
        if child.kind() == "if_expression" {
            cf_walk_if_at(child, depth, cf);
        } else {
            cf_walk(child, depth + 1, cf);
        }
    }
}

fn cf_walk_if_at(if_node: Node, depth: u32, cf: &mut ControlFlow) {
    cf.complexity += 1;
    cf.branch_points += 1;
    cf.reached(depth, if_node.start_position().row as u32);
    if let Some(cond) = if_node.child_by_field_name("condition") {
        cf_walk_from(cond, depth, cf);
    }
    if let Some(cons) = if_node.child_by_field_name("consequence") {
        cf_walk(cons, depth + 1, cf);
    }
    if let Some(alt) = if_node.child_by_field_name("alternative") {
        descend_else(alt, depth, cf);
    }
}

/// An explicit `return <expr>` anywhere in the body, not descending into
/// nested functions.
fn returns_value(node: Node) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let kind = child.kind();
        if FN_KINDS.contains(&kind) {
            continue;
        }
        if kind == "return_expression" && child.named_child_count() > 0 {
            return true;
        }
        if returns_value(child) {
            return true;
        }
    }
    false
}

/// The single meaningful statement/expression of a block, ignoring braces and
/// comments. `None` when the block holds anything else.
fn sole_item<'a>(block: Node<'a>) -> Option<Node<'a>> {
    let mut cursor = block.walk();
    let mut found = None;
    for child in block.named_children(&mut cursor) {
        if is_comment(child.kind()) {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = Some(child);
    }
    found
}

/// When the whole body is one delegating call, the callee as written, plus
/// whether inlining reduces to renaming the callee at the call site — the
/// callee is a bare name and the parameters are forwarded positionally,
/// unchanged and complete.
fn forwarding(body: Node, params: Option<Node>, src: &[u8]) -> (Option<String>, bool) {
    let mut item = match sole_item(body) {
        Some(n) => n,
        None => return (None, false),
    };
    // `return f(x);` nests the call two levels down; a tail `f(x)` is direct.
    if item.kind() == "expression_statement" {
        item = match item.named_child(0) {
            Some(n) => n,
            None => return (None, false),
        };
    }
    if item.kind() == "return_expression" {
        item = match item.named_child(0) {
            Some(n) => n,
            None => return (None, false),
        };
    }
    if item.kind() != "call_expression" {
        return (None, false);
    }
    let Some(callee) = item.child_by_field_name("function") else {
        return (None, false);
    };
    let Some(target) = text(callee, src) else {
        return (None, false);
    };
    let identity = callee.kind() == "identifier" && forwards_params_verbatim(item, params, src);
    (Some(target), identity)
}

/// The call's arguments are exactly this function's parameters, in order, as
/// bare names. A `self` receiver, a destructuring pattern, or any reordered or
/// wrapped argument disqualifies it.
fn forwards_params_verbatim(call: Node, params: Option<Node>, src: &[u8]) -> bool {
    let (Some(params), Some(args)) = (params, call.child_by_field_name("arguments")) else {
        return false;
    };
    let mut pc = params.walk();
    let names: Vec<String> = params
        .named_children(&mut pc)
        .map(|p| match p.kind() {
            "parameter" => p
                .child_by_field_name("pattern")
                .filter(|n| n.kind() == "identifier")
                .and_then(|n| text(n, src)),
            _ => None, // self_parameter, variadic, pattern destructuring
        })
        .collect::<Option<Vec<_>>>()
        .unwrap_or_default();
    if names.is_empty() {
        return false;
    }
    let mut ac = args.walk();
    let passed: Vec<String> = args
        .named_children(&mut ac)
        .filter(|n| !is_comment(n.kind()))
        .map(|n| (n.kind() == "identifier").then(|| text(n, src)).flatten())
        .collect::<Option<Vec<_>>>()
        .unwrap_or_default();
    passed == names
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
        let foo = &facts[0];
        assert_eq!(foo.param_count, 2);
        assert!(foo.returns_value);
        assert_eq!(foo.signature, "fn foo(a: i32, b: i32) -> i32");
        // `&self` counts, matching Python counting `self`.
        assert_eq!(facts[1].param_count, 2);
    }

    /// A multi-line right-hand side must stay one statement: if it split, a
    /// callee argument on a continuation line would land in the wrong component.
    #[test]
    fn stmt_spans_group_multi_line_statements() {
        let src = "fn f(a: i32) -> i32 {\n    let x = 1;\n    // noise\n    let y = wrap(\n        a,\n        x,\n    );\n    y\n}\n";
        assert_eq!(only(src).stmt_spans, vec![(1, 1), (3, 6), (7, 7)]);
    }

    #[test]
    fn complexity_counts_branches_booleans_and_try() {
        let src = "fn branchy(x: i32) -> i32 {\n    if x > 0 && x < 10 {\n        for i in 0..x { if i > 0 { return i; } }\n    } else if x < 0 || x == -5 {\n        while x > 0 { break; }\n    }\n    match x { 0 => 1, _ => 2 }\n}\n";
        let f = only(src);
        // if + && + for + if + else-if + || + while + 2 match arms = 9 over base 1
        assert_eq!(f.complexity, 10, "complexity {}", f.complexity);
        // structural: if, for, if, else-if, while, 2 arms = 7 (no booleans)
        assert_eq!(f.branch_points, 7, "branch_points {}", f.branch_points);
        // else-if is flat; deepest is for -> if = depth 3
        assert_eq!(f.max_nesting_depth, 3, "nesting {}", f.max_nesting_depth);
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
        assert_eq!(fb.comment_lines, 1);
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
    fn tail_call_forwards_and_is_rename_safe() {
        let f = only("fn load(path: &str) -> String {\n    read_file(path)\n}\n");
        assert_eq!(f.forward_target.as_deref(), Some("read_file"));
        assert!(f.forward_identity);
    }

    #[test]
    fn explicit_return_forwards_too() {
        let f = only("fn load(path: &str) -> String {\n    return read_file(path);\n}\n");
        assert_eq!(f.forward_target.as_deref(), Some("read_file"));
        assert!(f.forward_identity);
    }

    #[test]
    fn path_callee_forwards_but_is_not_rename_safe() {
        let f = only("fn join(a: &str, b: &str) -> String {\n    std::path::Path::join(a, b)\n}\n");
        assert_eq!(f.forward_target.as_deref(), Some("std::path::Path::join"));
        assert!(!f.forward_identity);
    }

    #[test]
    fn reordered_args_are_not_identity() {
        let f = only("fn swap(a: i32, b: i32) -> i32 {\n    add(b, a)\n}\n");
        assert_eq!(f.forward_target.as_deref(), Some("add"));
        assert!(!f.forward_identity);
    }

    #[test]
    fn broken_source_is_an_error_not_silent_facts() {
        assert!(analyze_rust("fn oops( {\n").is_err());
    }
}


