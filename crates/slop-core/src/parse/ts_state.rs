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

/// One `kind \x01 value \x02` record; byte-identical to the former `format!` encoding.
fn field(hasher: &mut blake3::Hasher, kind: &str, value: &[u8]) {
    hasher.update(kind.as_bytes()).update(b"\x01").update(value).update(b"\x02");
}

#[derive(Default)]
pub struct HashState {
    pub exact: blake3::Hasher,
    pub structural: blake3::Hasher,
    /// α-equivalence: bound names collapsed to their first-occurrence index,
    /// literals and free names kept. See [`Tok`].
    pub alpha: blake3::Hasher,
    pub significant: u32,
    /// First-occurrence index per bound name, so a consistent rename produces
    /// an identical digest.
    bound_index: HashMap<String, u32>,
}

impl HashState {
    /// Fold one leaf token into all three hashes.
    pub fn leaf(&mut self, kind: &str, text: &str, tok: Tok) {
        self.significant += 1;
        field(&mut self.exact, kind, text.as_bytes());

        // Structural: names *and* literals collapse — "same shape, renamed
        // variables / different constants".
        if matches!(tok, Tok::Bound(_) | Tok::Free | Tok::Literal) {
            self.structural.update(kind.as_bytes()).update(b"\x02");
        } else {
            field(&mut self.structural, kind, text.as_bytes());
        }
        self.alpha_leaf(kind, text, tok);
    }

    /// Fold a token into the α-hash only: signature tokens (defaults, annotations,
    /// decorators) change what a function means but not its body hashes.
    pub fn alpha_leaf(&mut self, kind: &str, text: &str, tok: Tok) {
        match tok {
            Tok::Bound(name) => {
                let next = self.bound_index.len() as u32;
                let idx = match self.bound_index.get(name) {
                    Some(idx) => *idx,
                    None => *self.bound_index.entry(name.to_string()).or_insert(next),
                };
                let mut digits = [0u8; 11];
                let mut cursor = std::io::Cursor::new(&mut digits[..]);
                let _ = std::io::Write::write_fmt(&mut cursor, format_args!("#{idx}"));
                let len = cursor.position() as usize;
                field(&mut self.alpha, kind, &digits[..len]);
            }
            // Everything else contributes verbatim: a different callee, a
            // different constant, or different syntax is a different function.
            _ => field(&mut self.alpha, kind, text.as_bytes()),
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

/// Structural decision points in `node`'s subtree, not descending into nested functions.
pub fn branch_points(node: Node, is_branch: fn(&str) -> bool, fn_kinds: &[&str]) -> u32 {
    let mut cursor = node.walk();
    let children: Vec<Node> = node.children(&mut cursor).filter(|c| !fn_kinds.contains(&c.kind())).collect();
    children.into_iter().map(|c| u32::from(is_branch(c.kind())) + branch_points(c, is_branch, fn_kinds)).sum()
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
    scopes: &[&str],
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
    // A nested scope binds for itself, and the hashing walk skips it too.
    for child in node.children(&mut cursor).filter(|child| !scopes.contains(&child.kind())) {
        collect_bound(child, src, binders, scopes, out);
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
