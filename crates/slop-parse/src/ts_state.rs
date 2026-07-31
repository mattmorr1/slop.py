//! Accumulator state shared by the tree-sitter extractors (JS/TS and Rust).
//! The walks themselves stay per-language — the node kinds and the constructs
//! that count as branches genuinely differ — but what they accumulate into
//! does not.

#[derive(Default)]
pub struct HashState {
    pub exact: blake3::Hasher,
    pub structural: blake3::Hasher,
    pub significant: u32,
    pub comment_lines: u32,
    pub code_lines: std::collections::BTreeSet<u32>,
}

impl HashState {
    /// Fold one leaf token into both hashes. `atom` collapses the token's text
    /// in the structural hash — that's what makes it "same shape, renamed".
    pub fn leaf(&mut self, kind: &str, text: &str, line: u32, atom: bool) {
        self.code_lines.insert(line);
        self.significant += 1;
        self.exact.update(format!("{kind}\u{1}{text}\u{2}").as_bytes());
        if atom {
            self.structural.update(format!("{kind}\u{2}").as_bytes());
        } else {
            self.structural.update(format!("{kind}\u{1}{text}\u{2}").as_bytes());
        }
    }

    /// The `(exact, structural)` pair, or empty strings when the body is below
    /// the significance floor and shouldn't participate in duplicate matching.
    pub fn finish(self, floor: u32) -> (String, String) {
        if self.significant >= floor {
            (
                self.exact.finalize().to_hex().to_string(),
                self.structural.finalize().to_hex().to_string(),
            )
        } else {
            (String::new(), String::new())
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
