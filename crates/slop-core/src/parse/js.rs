//! JavaScript / TypeScript per-function facts, via tree-sitter — the
//! second-language implementation behind [`crate::parse::Language`]. Mirrors the
//! Python extractor's semantics so the parser-based detectors (duplication,
//! purity, branch coverage) behave the same across languages:
//!
//! - exact hash = leaf kind + text (comments excluded) — "duplicate modulo
//!   comments/formatting";
//! - structural hash = leaf kind, with identifiers/literals collapsed —
//!   "same shape, renamed";
//! - `branch_points` = structural decision points only (no boolean operators).

use anyhow::{anyhow, Result};
use tree_sitter::{Node, Parser};

use crate::parse::ts_state::{branch_points, collect_bound, HashState, Tok};
use crate::parse::{FunctionFacts, MIN_SIGNIFICANT_TOKENS};

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
    analyze_with(
        source,
        if typescript {
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
        } else {
            tree_sitter_javascript::LANGUAGE.into()
        },
    )
}

pub fn analyze_tsx(source: &str) -> Result<Vec<FunctionFacts>> {
    analyze_with(source, tree_sitter_typescript::LANGUAGE_TSX.into())
}

fn analyze_with(source: &str, language: tree_sitter::Language) -> Result<Vec<FunctionFacts>> {
    let mut parser = Parser::new();
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

    let mut bound = std::collections::HashSet::new();
    collect_bound(node, src, BINDERS, FN_KINDS, &mut bound);
    let mut hasher = HashState::default();
    hash_walk(body, src, &bound, &mut hasher);

    let (body_hash, structural_hash, alpha_hash) = hasher.finish(MIN_SIGNIFICANT_TOKENS);

    FunctionFacts {
        name,
        name_line,
        start_line,
        end_line,
        branch_points: branch_points(body, is_branch_stmt, FN_KINDS),
        body_hash,
        structural_hash,
        alpha_hash,
        equiv_hash: String::new(),
        decorated: is_decorated(node),
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
/// bodies (their tokens belong to them).
fn hash_walk(node: Node, src: &[u8], bound: &std::collections::HashSet<String>, st: &mut HashState) {
    let kind = node.kind();
    if kind == "comment" {
        // A block comment spans multiple lines; a line comment is one.
        return;
    }
    if node.child_count() == 0 {
        // Leaf/token.
        if kind.trim().is_empty() {
            return;
        }
        let text = node.utf8_text(src).unwrap_or("");
        st.leaf(kind, text, classify(kind, text, bound));
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
    }

    #[test]
    fn branch_points_count_structural_branches_only() {
        let src = "function branchy(x) {\n  if (x > 0 && x < 10) {\n    for (const i of x) { if (i) x++; }\n  } else if (x < 0 || x === -5) {\n    while (x) x--;\n  }\n  return x ? 1 : 0;\n}\n";
        let facts = analyze_js(src, false).unwrap();
        // if, for, if, else-if, while = 5 (no booleans/ternary)
        assert_eq!(facts[0].branch_points, 5, "branch_points {}", facts[0].branch_points);
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

#[cfg(test)]
mod tsx_tests {
    use crate::parse::Language;

    /// JSX is a syntax error under the plain TypeScript grammar, which used to
    /// discard every React component file whole.
    #[test]
    fn tsx_components_parse_with_the_tsx_grammar() {
        let src = "export function Badge({ label }: { label: string }) {\n  const upper = label.toUpperCase();\n  return <span className=\"badge\">{upper}</span>;\n}\n";
        assert_eq!(Language::from_path("ui/Badge.tsx"), Some(Language::Tsx));
        assert!(Language::TypeScript.parse(src).is_err());
        let facts = Language::Tsx.parse(src).expect("tsx parses");
        assert_eq!(facts[0].name, "Badge");
    }
}

#[cfg(test)]
mod scope_tests {
    use super::analyze_js;

    /// Each reads a different global, which its callback parameter happens to shadow.
    /// Treating the callback's `limit` as bound in the outer function renamed
    /// the global reference and let different globals collide.
    #[test]
    fn a_nested_parameter_does_not_bind_the_outer_function() {
        let body = |global: &str| {
            format!("function f(xs) {{\n  const total = xs.length + {global};\n  xs.forEach(({global}) => log({global}));\n  return total * 2 + 1;\n}}\n")
        };
        let a = analyze_js(&body("limit"), false).unwrap();
        let b = analyze_js(&body("quota"), false).unwrap();
        let outer = |facts: &[crate::parse::FunctionFacts]| facts.iter().find(|f| f.name == "f").unwrap().alpha_hash.clone();
        assert!(!outer(&a).is_empty());
        assert_ne!(outer(&a), outer(&b), "different globals must not be alpha-equivalent");
    }
}
