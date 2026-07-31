//! JavaScript / TypeScript per-function facts, via tree-sitter — the
//! second-language implementation behind [`crate::Language`]. Mirrors the
//! Python extractor's semantics so the parser-based detectors (duplication,
//! complexity, over-commenting) behave the same across languages:
//!
//! - exact hash = leaf kind + text (comments excluded) — "duplicate modulo
//!   comments/formatting";
//! - structural hash = leaf kind, with identifiers/literals collapsed —
//!   "same shape, renamed";
//! - cyclomatic complexity = 1 + branch nodes (incl. `&&`/`||`/`??`/ternary);
//! - `branch_points` = structural decision points only (no boolean operators);
//! - nesting depth counts branching constructs, treating `else if` as flat.

use anyhow::{anyhow, Result};
use tree_sitter::{Node, Parser};

use crate::ts_state::{collect_bound, stmt_spans, ControlFlow, HashState, Tok};
use crate::{FunctionFacts, MIN_SIGNIFICANT_TOKENS};

/// Function-like nodes that carry a body worth analyzing.
const FN_KINDS: &[&str] = &[
    "function_declaration",
    "generator_function_declaration",
    "function_expression",
    "generator_function",
    "arrow_function",
    "method_definition",
];

/// Structural decision points (exclude boolean operators + ternary).
fn is_branch_stmt(kind: &str) -> bool {
    matches!(
        kind,
        "if_statement"
            | "for_statement"
            | "for_in_statement"
            | "while_statement"
            | "do_statement"
            | "catch_clause"
            | "switch_case"
    )
}


/// Node kinds that bind a name, paired with the field holding their pattern.
const BINDERS: &[(&str, &str)] = &[
    ("variable_declarator", "name"),
    ("required_parameter", "pattern"),
    ("optional_parameter", "pattern"),
    ("formal_parameters", ""),
    ("for_in_statement", "left"),
];

/// Classify a leaf for the three hashes. Property names stay free: they are
/// named by the object, not bound by this function.
fn classify<'a>(kind: &str, text: &'a str, bound: &std::collections::HashSet<String>) -> Tok<'a> {
    match kind {
        "identifier" if bound.contains(text) => Tok::Bound(text),
        "identifier" | "property_identifier" | "shorthand_property_identifier"
        | "shorthand_property_identifier_pattern" | "private_property_identifier"
        | "statement_identifier" | "type_identifier" => Tok::Free,
        "number" | "string_fragment" | "template_string" | "regex_pattern" => Tok::Literal,
        _ => Tok::Other,
    }
}

pub fn analyze_js(source: &str, typescript: bool) -> Result<Vec<FunctionFacts>> {
    let mut parser = Parser::new();
    let language = if typescript {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
    } else {
        tree_sitter_javascript::LANGUAGE.into()
    };
    parser
        .set_language(&language)
        .map_err(|e| anyhow!("loading tree-sitter grammar: {e}"))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow!("tree-sitter failed to parse"))?;
    // tree-sitter always returns a tree, error nodes and all. Callers that gate
    // a rewrite on "does this still parse" need the failure, not a partial tree.
    if tree.root_node().has_error() {
        return Err(anyhow!("source does not parse as JavaScript/TypeScript"));
    }

    let src = source.as_bytes();
    let mut facts = Vec::new();
    collect(tree.root_node(), src, &mut facts);
    Ok(facts)
}

/// Visit every node; emit facts for each named function-like node.
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

/// The bound name of a function-like node: its own `name` field, or the
/// binding it's assigned to (`const f = () => …`, `f: () => …`, class field).
/// `None` for a truly anonymous callback — those aren't graph entities and
/// would only add noise.
fn function_name<'a>(node: Node<'a>, src: &[u8]) -> Option<(String, Node<'a>)> {
    if let Some(name) = node.child_by_field_name("name") {
        return text(name, src).map(|t| (t, name));
    }
    let parent = node.parent()?;
    match parent.kind() {
        "variable_declarator" | "pair" | "public_field_definition" | "field_definition" => {
            let name = parent.child_by_field_name("name").or_else(|| parent.child_by_field_name("key"))?;
            text(name, src).map(|t| (t, name))
        }
        "assignment_expression" => {
            let left = parent.child_by_field_name("left")?;
            text(left, src).map(|t| (t, left))
        }
        _ => None,
    }
}

fn text(node: Node, src: &[u8]) -> Option<String> {
    node.utf8_text(src).ok().map(|s| s.to_string())
}

fn function_facts(node: Node, src: &[u8]) -> Option<FunctionFacts> {
    let (name, name_node) = function_name(node, src)?;
    let body = node.child_by_field_name("body")?;

    let start_line = node.start_position().row as u32;
    let end_line = node.end_position().row as u32;
    let name_line = name_node.start_position().row as u32;
    let signature = String::from_utf8_lossy(&src[node.start_byte()..body.start_byte()])
        .trim_end()
        .to_string();

    // Hashing + token/line counts over the body, skipping nested functions.
    let mut bound = std::collections::HashSet::new();
    collect_bound(node, src, BINDERS, &mut bound);
    let mut hasher = HashState::default();
    hash_walk(body, src, &bound, &mut hasher);

    let significant_tokens = hasher.significant;
    let comment_lines = hasher.comment_lines;
    let code_lines = hasher.code_lines.len() as u32;
    let (body_hash, structural_hash, alpha_hash) = hasher.finish(MIN_SIGNIFICANT_TOKENS);

    // Control-flow shape over the body.
    let mut cf = ControlFlow::default();
    cf_walk(body, 1, &mut cf);

    let param_count = node
        .child_by_field_name("parameters")
        .map(|p| p.named_child_count() as u32)
        .unwrap_or(0);

    FunctionFacts {
        name,
        name_line,
        start_line,
        end_line,
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
        decorated: is_decorated(node),
        param_count,
        returns_value: returns_value(body),
        forward_target: None,
        forward_identity: false,
    }
    .into()
}

/// A decorator precedes the definition (or its parent, for class fields).
fn is_decorated(node: Node) -> bool {
    let mut sib = node.prev_sibling();
    while let Some(s) = sib {
        if s.kind() == "decorator" {
            return true;
        }
        sib = s.prev_sibling();
    }
    node.parent()
        .map(|p| {
            let mut sib = p.prev_sibling();
            while let Some(s) = sib {
                if s.kind() == "decorator" {
                    return true;
                }
                sib = s.prev_sibling();
            }
            false
        })
        .unwrap_or(false)
}

/// Walk the body's leaf tokens into the hashers, skipping nested function
/// bodies (their tokens belong to them) and counting comment vs code lines.
fn hash_walk(node: Node, src: &[u8], bound: &std::collections::HashSet<String>, st: &mut HashState) {
    let kind = node.kind();
    if kind == "comment" {
        // A block comment spans multiple lines; a line comment is one.
        st.comment_lines += node.end_position().row as u32 - node.start_position().row as u32 + 1;
        return;
    }
    if node.child_count() == 0 {
        // Leaf/token.
        if kind.trim().is_empty() {
            return;
        }
        let text = node.utf8_text(src).unwrap_or("");
        st.leaf(kind, text, node.start_position().row as u32, classify(kind, text, bound));
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        // Don't descend into a nested function — its tokens are its own.
        if FN_KINDS.contains(&child.kind()) {
            continue;
        }
        hash_walk(child, src, bound, st);
    }
}

/// Count cyclomatic complexity, structural branch points, and nesting depth
/// over a subtree, skipping nested functions. `else if` is kept flat (the
/// chained `if` stays at the parent's depth), matching the Python extractor.
fn cf_walk(node: Node, depth: u32, cf: &mut ControlFlow) {
    if cf.complexity == 0 {
        cf.complexity = 1; // base path, set once at the body root
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let kind = child.kind();
        if FN_KINDS.contains(&kind) {
            continue; // nested function: its own scope
        }

        // Boolean operators and ternaries add paths (complexity) but aren't
        // structural branch points.
        if kind == "ternary_expression" {
            cf.complexity += 1;
        }
        if kind == "binary_expression" {
            if let Some(op) = child.child_by_field_name("operator") {
                if matches!(op.kind(), "&&" | "||" | "??") {
                    cf.complexity += 1;
                }
            }
        }

        if is_branch_stmt(kind) {
            cf.complexity += 1;
            cf.branch_points += 1;
            cf.reached(depth, child.start_position().row as u32);

            if kind == "if_statement" {
                // condition (for &&/||) at this level; consequence deeper;
                // `else if` stays flat, plain `else` deeper.
                if let Some(cond) = child.child_by_field_name("condition") {
                    cf_walk(cond, depth, cf);
                }
                if let Some(cons) = child.child_by_field_name("consequence") {
                    cf_walk(cons, depth + 1, cf);
                }
                if let Some(alt) = child.child_by_field_name("alternative") {
                    descend_else(alt, depth, cf);
                }
                continue;
            }
            // Other branches: their whole subtree is one level deeper.
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
        if child.kind() == "if_statement" {
            cf_walk_if_at(child, depth, cf); // chained else-if, same level
        } else {
            cf_walk(child, depth + 1, cf);
        }
    }
}

/// Process an `if_statement` reached as an `else if` — counted, but at the
/// current depth rather than one deeper.
fn cf_walk_if_at(if_node: Node, depth: u32, cf: &mut ControlFlow) {
    cf.complexity += 1;
    cf.branch_points += 1;
    cf.reached(depth, if_node.start_position().row as u32);
    if let Some(cond) = if_node.child_by_field_name("condition") {
        cf_walk(cond, depth, cf);
    }
    if let Some(cons) = if_node.child_by_field_name("consequence") {
        cf_walk(cons, depth + 1, cf);
    }
    if let Some(alt) = if_node.child_by_field_name("alternative") {
        descend_else(alt, depth, cf);
    }
}

/// A `return <expr>;` anywhere in the body, not descending into nested funcs.
fn returns_value(node: Node) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let kind = child.kind();
        if FN_KINDS.contains(&kind) {
            continue;
        }
        if kind == "return_statement" && child.named_child_count() > 0 {
            return true;
        }
        if returns_value(child) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_named_functions_and_methods() {
        let src = "function foo(a, b) {\n  return a + b;\n}\nclass S {\n  bar(x) { return x; }\n}\nconst baz = (y) => y * 2;\n";
        let facts = analyze_js(src, false).unwrap();
        let names: Vec<&str> = facts.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"foo"));
        assert!(names.contains(&"bar"));
        assert!(names.contains(&"baz"));
        let foo = facts.iter().find(|f| f.name == "foo").unwrap();
        assert_eq!(foo.param_count, 2);
        assert!(foo.returns_value);
    }

    /// A multi-line right-hand side stays one statement; an expression-bodied
    /// arrow is one statement rather than a partition of its subexpressions.
    #[test]
    fn stmt_spans_group_multi_line_and_expression_bodies() {
        let src = "function f(a) {\n  const x = 1;\n  // noise\n  const y = wrap(\n    a,\n    x,\n  );\n  return y;\n}\nconst g = (z) => z * 2;\n";
        let facts = analyze_js(src, false).unwrap();
        let f = facts.iter().find(|f| f.name == "f").unwrap();
        assert_eq!(f.stmt_spans, vec![(1, 1), (3, 6), (7, 7)]);
        let g = facts.iter().find(|f| f.name == "g").unwrap();
        assert_eq!(g.stmt_spans, vec![(9, 9)]);
    }

    #[test]
    fn complexity_counts_branches_and_booleans() {
        let src = "function branchy(x) {\n  if (x > 0 && x < 10) {\n    for (const i of x) { if (i) x++; }\n  } else if (x < 0 || x === -5) {\n    while (x) x--;\n  }\n  return x ? 1 : 0;\n}\n";
        let facts = analyze_js(src, false).unwrap();
        let f = &facts[0];
        // if + && + for + if + elif + || + while + ternary = 8 over base 1 => 9
        assert!(f.complexity >= 8, "complexity {}", f.complexity);
        // structural branches: if, for, if, else-if, while = 5 (no booleans/ternary)
        assert_eq!(f.branch_points, 5, "branch_points {}", f.branch_points);
        // else-if is flat; deepest is for->if = depth 3
        assert_eq!(f.max_nesting_depth, 3, "nesting {}", f.max_nesting_depth);
    }

    #[test]
    fn structural_hash_ignores_names_exact_ignores_comments() {
        let a = "function clean(rows) {\n  const out = [];\n  for (const r of rows) { if (r) out.push(r.trim()); }\n  return out;\n}\n";
        let b = "function scale(items) {\n  // different comment\n  const acc = [];\n  for (const x of items) { if (x) acc.push(x.trim()); }\n  return acc;\n}\n";
        let fa = analyze_js(a, false).unwrap();
        let fb = analyze_js(b, false).unwrap();
        assert!(!fa[0].structural_hash.is_empty());
        // Same shape, renamed vars -> structural match; exact bodies differ.
        assert_eq!(fa[0].structural_hash, fb[0].structural_hash);
        assert_ne!(fa[0].body_hash, fb[0].body_hash);
    }

    #[test]
    fn typescript_parses() {
        let src = "function typed(x: number): number {\n  if (x > 0) { return x; }\n  return 0;\n}\n";
        let facts = analyze_js(src, true).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].name, "typed");
        assert_eq!(facts[0].branch_points, 1);
    }
}
