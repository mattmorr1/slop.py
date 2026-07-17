//! Token/AST facts per function, for the parser-dependent detectors:
//! Tier-1/Tier-2 duplication, complexity spikes, over-commenting.
//!
//! Tier-1 hash = token kinds + source text, comments/blank-lines excluded:
//! "exact duplicate modulo comments and whitespace."
//! Tier-2 hash = token kinds only, with names/literals collapsed to
//! placeholders: "same shape, renamed variables / different constants."

use anyhow::{Context, Result};
use ruff_python_ast::token::{Token, TokenKind};
use ruff_python_ast::{self as ast, Expr, Stmt};
use ruff_python_parser::parse_module;
use ruff_text_size::{Ranged, TextRange, TextSize};

mod js;

/// A source language slop can produce per-function facts for. The parser-based
/// detectors (duplication, complexity, over-commenting) need a language-aware
/// parser; the graph/effect detectors work over SCIP for *any* indexed
/// language regardless of what's here. Adding a language = a new variant, an
/// extension in [`Language::from_path`], and a parse arm — the rest of the
/// pipeline is already language-agnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Language {
    Python,
    JavaScript,
    TypeScript,
}

impl Language {
    /// The language of a repo-relative path by extension, or `None` when no
    /// parser exists for it (the file's graph/effect facts still come from
    /// SCIP — only the parser-based facts are skipped).
    pub fn from_path(path: &str) -> Option<Language> {
        match path.rsplit('.').next() {
            Some("py" | "pyi") => Some(Language::Python),
            Some("js" | "jsx" | "mjs" | "cjs") => Some(Language::JavaScript),
            Some("ts" | "tsx" | "mts" | "cts") => Some(Language::TypeScript),
            _ => None,
        }
    }

    /// Per-function facts for `source` in this language.
    pub fn parse(self, source: &str) -> Result<Vec<FunctionFacts>> {
        match self {
            Language::Python => analyze_file(source),
            Language::JavaScript => js::analyze_js(source, false),
            Language::TypeScript => js::analyze_js(source, true),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FunctionFacts {
    pub name: String,
    /// 0-based line of the `def` name token — the join key against the
    /// graph's SCIP-derived definition lines.
    pub name_line: u32,
    pub start_line: u32,
    pub end_line: u32,
    /// Raw source from the `def` (or leading decorator) through the `:` —
    /// the type-annotated header used verbatim in skeletons.
    pub signature: String,
    /// Token-based cyclomatic complexity: 1 + branch keywords (includes the
    /// `and`/`or` boolean operators McCabe counts).
    pub complexity: u32,
    /// Structural control-flow decision points only — `if`/`elif`/`for`/
    /// `while`/`except`/`case`, *excluding* boolean operators. This is the
    /// "how many independent branches" signal, uninflated by a single fat
    /// boolean guard (which drives most of the low-end complexity noise).
    pub branch_points: u32,
    /// Deepest nesting of branching constructs in the body (function body
    /// statements are depth 1). The "how tangled" signal an agent refactor
    /// actually keys off.
    pub max_nesting_depth: u32,
    /// 0-based line of the statement that first reaches `max_nesting_depth` —
    /// the concrete locus fix guidance points at. 0 when nothing nests.
    pub deepest_line: u32,
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
    /// Total parameter count (self included) — Tier-3 bucket key component.
    pub param_count: u32,
    /// Body contains `return <expr>` at any depth.
    pub returns_value: bool,
    /// When the whole body is exactly `return CALLEE(args)`, the callee as
    /// written (`read_file`, `os.path.join`); `None` otherwise. The thin-wrapper
    /// signal — a one-statement function that only delegates.
    pub forward_target: Option<String>,
    /// The delegation passes this function's parameters to the callee
    /// positionally, unchanged and complete, with a bare-name callee and no
    /// defaults/varargs in play — so inlining reduces to renaming the callee at
    /// the call site. `false` when args are reordered, bound, partial, or the
    /// callee is dotted.
    pub forward_identity: bool,
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

/// Structural control-flow shape of a function body, computed from the AST
/// (not tokens) so nesting is exact. Decision points and nesting are counted
/// only for genuine branching constructs — `if`/`elif`/`for`/`while`/`except`/
/// `case` — while `with`/`try`-body wrappers pass depth through unchanged
/// (they nest visually but branch nothing). Nested `def`/`class` are their own
/// scope and are not descended into.
#[derive(Default)]
struct ControlFlow {
    branch_points: u32,
    max_depth: u32,
    deepest_line: u32,
}

impl ControlFlow {
    /// Record that a branching statement sits at `depth`; remember the line of
    /// the first statement to reach a new maximum.
    fn reached(&mut self, depth: u32, line: u32) {
        if depth > self.max_depth {
            self.max_depth = depth;
            self.deepest_line = line;
        }
    }
}

fn control_flow(stmts: &[Stmt], depth: u32, lines: &LineIndex, cf: &mut ControlFlow) {
    for stmt in stmts {
        let line = |s: &dyn Ranged| lines.line(s.range().start());
        match stmt {
            Stmt::If(s) => {
                cf.branch_points += 1;
                cf.reached(depth, line(s));
                control_flow(&s.body, depth + 1, lines, cf);
                for clause in &s.elif_else_clauses {
                    // `elif` is a decision point; a bare `else` is not.
                    if clause.test.is_some() {
                        cf.branch_points += 1;
                    }
                    control_flow(&clause.body, depth + 1, lines, cf);
                }
            }
            Stmt::For(s) => {
                cf.branch_points += 1;
                cf.reached(depth, line(s));
                control_flow(&s.body, depth + 1, lines, cf);
                control_flow(&s.orelse, depth + 1, lines, cf);
            }
            Stmt::While(s) => {
                cf.branch_points += 1;
                cf.reached(depth, line(s));
                control_flow(&s.body, depth + 1, lines, cf);
                control_flow(&s.orelse, depth + 1, lines, cf);
            }
            Stmt::Match(s) => {
                cf.reached(depth, line(s));
                for case in &s.cases {
                    cf.branch_points += 1;
                    control_flow(&case.body, depth + 1, lines, cf);
                }
            }
            Stmt::Try(s) => {
                // The wrapper doesn't branch; each `except` does.
                control_flow(&s.body, depth, lines, cf);
                for handler in &s.handlers {
                    let ast::ExceptHandler::ExceptHandler(h) = handler;
                    cf.branch_points += 1;
                    cf.reached(depth, lines.line(h.range().start()));
                    control_flow(&h.body, depth + 1, lines, cf);
                }
                control_flow(&s.orelse, depth, lines, cf);
                control_flow(&s.finalbody, depth, lines, cf);
            }
            // `with` nests visually but introduces no branch — pass through.
            Stmt::With(s) => control_flow(&s.body, depth, lines, cf),
            // Nested defs/classes are separate scopes; their complexity is
            // attributed to them, not the enclosing function.
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) => {}
            _ => {}
        }
    }
}

pub(crate) const MIN_SIGNIFICANT_TOKENS: u32 = 20;

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

    let params = &func.parameters;
    let param_count = (params.posonlyargs.len()
        + params.args.len()
        + params.kwonlyargs.len()
        + usize::from(params.vararg.is_some())
        + usize::from(params.kwarg.is_some())) as u32;

    let signature = source[TextRange::new(full.start(), body.start())]
        .trim_end()
        .to_string();

    let mut cf = ControlFlow::default();
    control_flow(&func.body, 1, lines, &mut cf);

    let (forward_target, forward_identity) = forwarder(func);

    FunctionFacts {
        name: func.name.to_string(),
        name_line: lines.line(func.name.range().start()),
        start_line: lines.line(full.start()),
        end_line: lines.line(full.end()),
        signature,
        complexity,
        branch_points: cf.branch_points,
        max_nesting_depth: cf.max_depth,
        deepest_line: cf.deepest_line,
        body_hash,
        structural_hash,
        significant_tokens: significant,
        comment_lines,
        code_lines: code_line_set.len() as u32,
        decorated: !func.decorator_list.is_empty(),
        param_count,
        returns_value: body_returns_value(&func.body),
        forward_target,
        forward_identity,
    }
}

/// Detect the thin-wrapper shape: a body that is exactly `return CALLEE(args)`.
/// Returns the callee as written and whether the delegation is a pure positional
/// pass-through of the function's own parameters (the rename-safe inline case).
fn forwarder(func: &ast::StmtFunctionDef) -> (Option<String>, bool) {
    let [Stmt::Return(ret)] = func.body.as_slice() else {
        return (None, false);
    };
    let Some(Expr::Call(call)) = ret.value.as_deref() else {
        return (None, false);
    };
    let Some(callee) = callee_path(&call.func) else {
        return (None, false);
    };
    let identity = callee_path_is_bare(&call.func) && args_are_params_verbatim(func, call);
    (Some(callee), identity)
}

/// Render `foo` / `pkg.mod.foo` from a call target; `None` for anything not a
/// plain name or attribute chain (subscripts, calls, lambdas).
fn callee_path(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Name(n) => Some(n.id.to_string()),
        Expr::Attribute(a) => Some(format!("{}.{}", callee_path(&a.value)?, a.attr)),
        _ => None,
    }
}

fn callee_path_is_bare(expr: &Expr) -> bool {
    matches!(expr, Expr::Name(_))
}

/// The call passes exactly the function's positional parameters, in order, by
/// name — and the signature carries nothing (defaults, `*args`, `**kwargs`,
/// keyword-only) that would make a bare callee-rename change behavior.
fn args_are_params_verbatim(func: &ast::StmtFunctionDef, call: &ast::ExprCall) -> bool {
    let p = &func.parameters;
    let clean = p.kwonlyargs.is_empty() && p.vararg.is_none() && p.kwarg.is_none();
    let positional: Vec<&str> = p
        .posonlyargs
        .iter()
        .chain(p.args.iter())
        .filter(|pd| pd.default.is_none())
        .map(|pd| pd.parameter.name.as_str())
        .collect();
    let all_positional_defaultless =
        positional.len() == p.posonlyargs.len() + p.args.len();
    if !clean || !all_positional_defaultless {
        return false;
    }
    let a = &call.arguments;
    if !a.keywords.is_empty() || a.args.len() != positional.len() {
        return false;
    }
    a.args.iter().zip(&positional).all(|(arg, name)| {
        matches!(arg, Expr::Name(n) if n.id.as_str() == *name)
    })
}

fn body_returns_value(stmts: &[Stmt]) -> bool {
    for stmt in stmts {
        let found = match stmt {
            Stmt::Return(r) => r.value.is_some(),
            // Nested function defs are their own scope — don't descend.
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) => false,
            Stmt::If(s) => {
                body_returns_value(&s.body)
                    || s.elif_else_clauses
                        .iter()
                        .any(|c| body_returns_value(&c.body))
            }
            Stmt::While(s) => body_returns_value(&s.body) || body_returns_value(&s.orelse),
            Stmt::For(s) => body_returns_value(&s.body) || body_returns_value(&s.orelse),
            Stmt::With(s) => body_returns_value(&s.body),
            Stmt::Try(s) => {
                body_returns_value(&s.body)
                    || s.handlers.iter().any(|h| {
                        let ast::ExceptHandler::ExceptHandler(h) = h;
                        body_returns_value(&h.body)
                    })
                    || body_returns_value(&s.orelse)
                    || body_returns_value(&s.finalbody)
            }
            _ => false,
        };
        if found {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn only(src: &str) -> FunctionFacts {
        let mut f = analyze_file(src).unwrap();
        assert_eq!(f.len(), 1, "expected exactly one function");
        f.pop().unwrap()
    }

    #[test]
    fn pure_positional_forward_is_identity() {
        let f = only("def load(path):\n    return read_file(path)\n");
        assert_eq!(f.forward_target.as_deref(), Some("read_file"));
        assert!(f.forward_identity);
    }

    #[test]
    fn dotted_callee_forwards_but_is_not_rename_safe() {
        let f = only("def join(a, b):\n    return os.path.join(a, b)\n");
        assert_eq!(f.forward_target.as_deref(), Some("os.path.join"));
        assert!(!f.forward_identity);
    }

    #[test]
    fn reordered_or_bound_args_are_not_identity() {
        let swap = only("def f(a, b):\n    return g(b, a)\n");
        assert_eq!(swap.forward_target.as_deref(), Some("g"));
        assert!(!swap.forward_identity);
        let bound = only("def f(a, b):\n    return g(a, b, mode=1)\n");
        assert!(!bound.forward_identity);
    }

    #[test]
    fn defaults_and_varargs_block_identity() {
        let deflt = only("def f(a, b=2):\n    return g(a, b)\n");
        assert!(!deflt.forward_identity);
        let star = only("def f(*args):\n    return g(*args)\n");
        assert!(!star.forward_identity);
    }

    #[test]
    fn multi_statement_body_is_not_a_forwarder() {
        let f = only("def f(a):\n    x = a + 1\n    return g(x)\n");
        assert_eq!(f.forward_target, None);
        assert!(!f.forward_identity);
    }

    #[test]
    fn non_call_return_is_not_a_forwarder() {
        let f = only("def name(self):\n    return self._name\n");
        assert_eq!(f.forward_target, None);
    }

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
    fn control_flow_shape_separates_nesting_from_boolean_density() {
        // A single fat boolean guard: high cyclomatic, but flat and few
        // branch points — the noise class we no longer want to flag hard.
        let flat = analyze_file(
            "def guard(a, b, c, d):\n    if a and b and c and d and a or b:\n        return 1\n    return 0\n",
        )
        .unwrap();
        assert!(flat[0].complexity >= 6, "boolean ops inflate cyclomatic");
        assert_eq!(flat[0].branch_points, 1, "one structural decision point");
        assert_eq!(flat[0].max_nesting_depth, 1);

        // Genuinely tangled: three levels of nested branching.
        let tangled = analyze_file(
            "def deep(xs):\n    for x in xs:\n        if x:\n            while x:\n                x -= 1\n    return xs\n",
        )
        .unwrap();
        assert_eq!(tangled[0].branch_points, 3); // for + if + while
        assert_eq!(tangled[0].max_nesting_depth, 3);
        // deepest_line points at the `while` (0-based line 3).
        assert_eq!(tangled[0].deepest_line, 3);
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
