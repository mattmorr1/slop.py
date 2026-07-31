//! Accumulator state shared by the tree-sitter extractors (JS/TS and Rust).
//! The walks themselves stay per-language — the node kinds and the constructs
//! that count as branches genuinely differ — but what they accumulate into
//! does not.

use std::collections::{HashMap, HashSet};

use tree_sitter::Node;

/// What a leaf token is, for the purposes of the three hashes. The distinction
/// between a *bound* and a *free* identifier is the whole basis of the
/// α-equivalence hash: renaming a local is a rename, but swapping one callee for
/// another is a different function.
pub enum Tok<'a> {
    /// An identifier bound inside this function — a parameter or a local.
    /// Consistently renaming it does not change what the function does.
    Bound(&'a str),
    /// An identifier resolved outside the function: a callee, an import, a
    /// global, an attribute name. Part of the meaning, never collapsed.
    Free,
    /// A number, string or char literal.
    Literal,
    /// Punctuation, keywords, operators.
    Other,
}

#[derive(Default)]
pub struct HashState {
    pub exact: blake3::Hasher,
    pub structural: blake3::Hasher,
    /// α-equivalence: bound names collapsed to their first-occurrence index,
    /// literals and free names kept. See [`Tok`].
    pub alpha: blake3::Hasher,
    pub significant: u32,
    pub comment_lines: u32,
    pub code_lines: std::collections::BTreeSet<u32>,
    /// First-occurrence index per bound name, so a consistent rename produces
    /// an identical digest.
    bound_index: HashMap<String, u32>,
}

impl HashState {
    /// Fold one leaf token into all three hashes.
    pub fn leaf(&mut self, kind: &str, text: &str, line: u32, tok: Tok) {
        self.code_lines.insert(line);
        self.significant += 1;
        self.exact.update(format!("{kind}\u{1}{text}\u{2}").as_bytes());

        // Structural: names *and* literals collapse — "same shape, renamed
        // variables / different constants".
        let atom = matches!(tok, Tok::Bound(_) | Tok::Free | Tok::Literal);
        if atom {
            self.structural.update(format!("{kind}\u{2}").as_bytes());
        } else {
            self.structural.update(format!("{kind}\u{1}{text}\u{2}").as_bytes());
        }

        match tok {
            Tok::Bound(name) => {
                let next = self.bound_index.len() as u32;
                let idx = *self.bound_index.entry(name.to_string()).or_insert(next);
                self.alpha.update(format!("{kind}\u{1}#{idx}\u{2}").as_bytes());
            }
            // Everything else contributes verbatim: a different callee, a
            // different constant, or different syntax is a different function.
            _ => {
                self.alpha.update(format!("{kind}\u{1}{text}\u{2}").as_bytes());
            }
        }
    }

    /// The `(exact, structural, alpha)` triple, or empty strings when the body
    /// is below the significance floor and shouldn't participate in duplicate
    /// matching.
    pub fn finish(self, floor: u32) -> (String, String, String) {
        if self.significant >= floor {
            (
                self.exact.finalize().to_hex().to_string(),
                self.structural.finalize().to_hex().to_string(),
                self.alpha.finalize().to_hex().to_string(),
            )
        } else {
            (String::new(), String::new(), String::new())
        }
    }
}

#[derive(Default)]
pub struct ControlFlow {
    pub complexity: u32,
    pub branch_points: u32,
    pub max_depth: u32,
    pub deepest_line: u32,
}

impl ControlFlow {
    pub fn reached(&mut self, depth: u32, line: u32) {
        if depth > self.max_depth {
            self.max_depth = depth;
            self.deepest_line = line;
        }
    }
}

/// Inclusive line spans of a body's top-level statements, in source order.
/// Comments are skipped: they hold no occurrences, so counting them as
/// statements would only inflate a component's size.
///
/// An expression-bodied arrow (`x => x + 1`) is one statement, not a partition
/// of its own subexpressions — hence the block check rather than descending
/// unconditionally.
pub fn stmt_spans(body: Node) -> Vec<(u32, u32)> {
    let span = |n: &Node| (n.start_position().row as u32, n.end_position().row as u32);
    if !body.kind().ends_with("block") {
        return vec![span(&body)];
    }
    let mut cursor = body.walk();
    body.named_children(&mut cursor)
        .filter(|c| !c.kind().contains("comment"))
        .map(|c| span(&c))
        .collect()
}

/// Identifiers a function binds locally, for the α-equivalence hash.
///
/// `binders` pairs a node kind that introduces a binding with the field its
/// pattern lives in; an empty field means the node's whole subtree is pattern.
/// Reading only the pattern field matters: `let x = compute(y)` binds `x`, and
/// collecting the whole subtree would wrongly mark `compute` and `y` as bound —
/// which would collapse a callee and let the hash match two different functions.
pub fn collect_bound(
    node: Node,
    src: &[u8],
    binders: &[(&str, &str)],
    out: &mut HashSet<String>,
) {
    if let Some((_, field)) = binders.iter().find(|(kind, _)| *kind == node.kind()) {
        let pattern = if field.is_empty() {
            Some(node)
        } else {
            node.child_by_field_name(field)
        };
        if let Some(pattern) = pattern {
            collect_identifiers(pattern, src, out);
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_bound(child, src, binders, out);
    }
}

/// Every `identifier` in a subtree — used on pattern subtrees, so tuple and
/// struct destructuring binds each name it names.
fn collect_identifiers(node: Node, src: &[u8], out: &mut HashSet<String>) {
    if node.kind() == "identifier" {
        if let Ok(t) = node.utf8_text(src) {
            out.insert(t.to_string());
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_identifiers(child, src, out);
    }
}
