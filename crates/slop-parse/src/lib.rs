//! Token/AST facts per function, for the parser-dependent detectors:
//! Tier-1/Tier-2 duplication, complexity spikes, over-commenting.
//!
//! Tier-1 hash = token kinds + source text, comments/blank-lines excluded:
//! "exact duplicate modulo comments and whitespace."
//! Tier-2 hash = token kinds only, with names/literals collapsed to
//! placeholders: "same shape, renamed variables / different constants."

use anyhow::{Context, Result};
use ruff_python_ast::token::{Token, TokenKind};
use ruff_python_ast::{self as ast, Stmt};
use ruff_python_parser::parse_module;
use ruff_text_size::{Ranged, TextRange, TextSize};

#[derive(Debug, Clone)]
pub struct FunctionFacts {
    pub name: String,
    /// 0-based line of the `def` name token — the join key against the
    /// graph's SCIP-derived definition lines.
    pub name_line: u32,
    pub start_line: u32,
    pub end_line: u32,
    /// Token-based cyclomatic complexity: 1 + branch keywords.
    pub complexity: u32,
    /// Blake3, hex. Empty when the body is below the significance floor.
    pub body_hash: String,
    pub structural_hash: String,
    /// Number of body tokens that fed the hashes (significance measure).
    pub significant_tokens: u32,
    pub comment_lines: u32,
    pub code_lines: u32,
    /// Any decorator present: the function may be framework-registered
    /// (routes, MCP handlers, fixtures) and called without a by-name ref.
    pub decorated: bool,
}

struct LineIndex(Vec<TextSize>);

impl LineIndex {
    fn new(source: &str) -> Self {
        let mut starts = vec![TextSize::from(0)];
        for (i, b) in source.bytes().enumerate() {
            if b == b'\n' {
                starts.push(TextSize::from(i as u32 + 1));
            }
        }
        Self(starts)
    }
    fn line(&self, offset: TextSize) -> u32 {
        (self.0.partition_point(|&s| s <= offset) - 1) as u32
    }
}

/// Tokens that never contribute to either hash.
fn is_trivia(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Comment | TokenKind::NonLogicalNewline | TokenKind::Newline
    ) || kind == TokenKind::EndOfFile
}

fn is_atom(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Name
            | TokenKind::Int
            | TokenKind::Float
            | TokenKind::Complex
            | TokenKind::String
            | TokenKind::FStringStart
            | TokenKind::FStringMiddle
            | TokenKind::FStringEnd
    )
}

fn is_branch(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::If
            | TokenKind::Elif
            | TokenKind::For
            | TokenKind::While
            | TokenKind::Except
            | TokenKind::And
            | TokenKind::Or
            | TokenKind::Case
    )
}

pub fn analyze_file(source: &str) -> Result<Vec<FunctionFacts>> {
    let parsed = parse_module(source).context("parsing python module")?;
    let lines = LineIndex::new(source);
    let tokens = parsed.tokens();

    let mut facts = Vec::new();
    collect_functions(parsed.syntax().body.as_slice(), &mut |func| {
        facts.push(function_facts(func, source, &lines, tokens.as_ref()));
    });
    Ok(facts)
}

fn collect_functions<'a>(stmts: &'a [Stmt], visit: &mut impl FnMut(&'a ast::StmtFunctionDef)) {
    for stmt in stmts {
        match stmt {
            Stmt::FunctionDef(f) => {
                visit(f);
                collect_functions(&f.body, visit);
            }
            Stmt::ClassDef(c) => collect_functions(&c.body, visit),
            Stmt::If(s) => {
                collect_functions(&s.body, visit);
                for clause in &s.elif_else_clauses {
                    collect_functions(&clause.body, visit);
                }
            }
            Stmt::While(s) => {
                collect_functions(&s.body, visit);
                collect_functions(&s.orelse, visit);
            }
            Stmt::For(s) => {
                collect_functions(&s.body, visit);
                collect_functions(&s.orelse, visit);
            }
            Stmt::With(s) => collect_functions(&s.body, visit),
            Stmt::Try(s) => {
                collect_functions(&s.body, visit);
                for handler in &s.handlers {
                    let ast::ExceptHandler::ExceptHandler(h) = handler;
                    collect_functions(&h.body, visit);
                }
                collect_functions(&s.orelse, visit);
                collect_functions(&s.finalbody, visit);
            }
            _ => {}
        }
    }
}

/// Body range: from the first body statement to the end of the last —
/// excludes the signature so overloads with identical bodies still match.
fn body_range(func: &ast::StmtFunctionDef) -> TextRange {
    let first = func.body.first().map(|s| s.range().start());
    let last = func.body.last().map(|s| s.range().end());
    match (first, last) {
        (Some(a), Some(b)) => TextRange::new(a, b),
        _ => func.range(),
    }
}

const MIN_SIGNIFICANT_TOKENS: u32 = 20;

fn function_facts(
    func: &ast::StmtFunctionDef,
    source: &str,
    lines: &LineIndex,
    tokens: &[Token],
) -> FunctionFacts {
    let body = body_range(func);
    let mut exact = blake3::Hasher::new();
    let mut structural = blake3::Hasher::new();
    let mut significant = 0u32;
    let mut complexity = 1u32;
    let mut comment_lines = 0u32;
    let mut code_line_set: Vec<u32> = Vec::new();

    for token in tokens {
        if token.range().start() < body.start() || token.range().end() > body.end() {
            // Comments inside the function but outside statement ranges
            // (e.g. trailing) still count toward density via the full span.
            continue;
        }
        let kind = token.kind();
        if kind == TokenKind::Comment {
            comment_lines += 1;
            continue;
        }
        if is_trivia(kind) || matches!(kind, TokenKind::Indent | TokenKind::Dedent) {
            continue;
        }
        let line = lines.line(token.range().start());
        if code_line_set.last() != Some(&line) {
            code_line_set.push(line);
        }
        if is_branch(kind) {
            complexity += 1;
        }
        significant += 1;
        let text = &source[token.range()];
        exact.update(format!("{kind:?}\u{1}{text}\u{2}").as_bytes());
        if is_atom(kind) {
            structural.update(format!("{kind:?}\u{2}").as_bytes());
        } else {
            structural.update(format!("{kind:?}\u{1}{text}\u{2}").as_bytes());
        }
    }

    // Comments attached to the function but between statements/before the
    // body start on their own lines: count those within the whole fn span.
    let full = func.range();
    for token in tokens {
        if token.kind() == TokenKind::Comment
            && token.range().start() >= full.start()
            && token.range().end() <= full.end()
            && (token.range().start() < body.start() || token.range().end() > body.end())
        {
            comment_lines += 1;
        }
    }

    let (body_hash, structural_hash) = if significant >= MIN_SIGNIFICANT_TOKENS {
        (
            exact.finalize().to_hex().to_string(),
            structural.finalize().to_hex().to_string(),
        )
    } else {
        (String::new(), String::new())
    };

    FunctionFacts {
        name: func.name.to_string(),
        name_line: lines.line(func.name.range().start()),
        start_line: lines.line(full.start()),
        end_line: lines.line(full.end()),
        complexity,
        body_hash,
        structural_hash,
        significant_tokens: significant,
        comment_lines,
        code_lines: code_line_set.len() as u32,
        decorated: !func.decorator_list.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_bodies_hash_equal_despite_comments_and_names() {
        let src = r#"
def clean_rows(rows):
    out = []
    for row in rows:
        value = row.strip().lower()
        if value and value not in out:
            out.append(value)
    return out

def normalize_rows(rows):
    # completely different comments here
    out = []
    for row in rows:
        value = row.strip().lower()
        # another comment
        if value and value not in out:
            out.append(value)
    return out

def scale_rows(rows):
    acc = []
    for item in rows:
        scaled = item.strip().upper()
        if scaled and scaled not in acc:
            acc.append(scaled)
    return acc
"#;
        let facts = analyze_file(src).unwrap();
        assert_eq!(facts.len(), 3);
        let (a, b, c) = (&facts[0], &facts[1], &facts[2]);
        assert!(!a.body_hash.is_empty());
        // Tier-1: identical modulo comments.
        assert_eq!(a.body_hash, b.body_hash);
        // Tier-2: same shape, renamed vars + different method -> structural
        // equal to neither? strip vs strip + lower vs upper are Name tokens,
        // collapsed -> structurally identical.
        assert_eq!(a.structural_hash, c.structural_hash);
        // But not Tier-1 identical.
        assert_ne!(a.body_hash, c.body_hash);
    }

    #[test]
    fn complexity_counts_branches() {
        let src = r#"
def branchy(x):
    if x > 0 and x < 10:
        for i in range(x):
            if i % 2:
                x += 1
    elif x < 0 or x == -5:
        while x:
            x -= 1
    return x
"#;
        let facts = analyze_file(src).unwrap();
        // 1 + if + and + for + if + elif + or + while = 8
        assert_eq!(facts[0].complexity, 8);
    }

    #[test]
    fn comment_density_measured() {
        let src = r#"
def documented(x):
    # add one to x
    x = x + 1
    # multiply x by two
    x = x * 2
    # return the value of x
    return x
"#;
        let facts = analyze_file(src).unwrap();
        assert_eq!(facts[0].comment_lines, 3);
        assert_eq!(facts[0].code_lines, 3);
    }

    #[test]
    fn small_bodies_get_no_hash() {
        let facts = analyze_file("def tiny(x):\n    return x\n").unwrap();
        assert!(facts[0].body_hash.is_empty());
    }
}
