//! Token/AST facts per function, for the parser-dependent detectors:
//! Tier-1/Tier-2 duplication, complexity spikes, over-commenting.
//!
//! Tier-1 hash = token kinds + source text, comments/blank-lines excluded:
//! "exact duplicate modulo comments and whitespace."
//! Tier-2 hash = token kinds only, with names/literals collapsed to
//! placeholders: "same shape, renamed variables / different constants."

use std::collections::HashMap;

use anyhow::{Context, Result};
use ruff_python_ast::token::{Token, TokenKind};
use ruff_python_ast::{self as ast, Expr, Stmt};
use ruff_python_parser::parse_module;
use ruff_text_size::{Ranged, TextRange, TextSize};

use ts_state::Tok;

mod js;
pub mod names;
pub mod resources;
mod rust;
#[cfg(feature = "egraph")]
pub mod egraph;
pub mod equiv;
mod scope;
mod ts_state;

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
    /// `.tsx`: TypeScript plus JSX, a separate tree-sitter grammar because `<T>x` casts
    /// and JSX elements are ambiguous under one grammar.
    Tsx,
    Rust,
}

/// Every extension [`Language::from_path`] recognizes. The one list callers
/// outside this crate filter paths by — a diff pathspec, a staleness walk —
/// so a new language can't be supported here and invisible there.
pub const SOURCE_EXTS: &[&str] = &[
    "py", "pyi", "js", "jsx", "mjs", "cjs", "ts", "tsx", "mts", "cts", "rs",
];

impl Language {
    /// The language of a repo-relative path by extension, or `None` when no
    /// parser exists for it (the file's graph/effect facts still come from
    /// SCIP — only the parser-based facts are skipped).
    pub fn from_path(path: &str) -> Option<Language> {
        match path.rsplit('.').next() {
            Some("py" | "pyi") => Some(Language::Python),
            Some("js" | "jsx" | "mjs" | "cjs") => Some(Language::JavaScript),
            Some("ts" | "mts" | "cts") => Some(Language::TypeScript),
            Some("tsx") => Some(Language::Tsx),
            Some("rs") => Some(Language::Rust),
            _ => None,
        }
    }

    /// The line-comment prefix, plus the prefixes that start with it but carry
    /// meaning (doc comments) and must survive a comment strip. Rust's `#` is
    /// deliberately absent: `#[derive]` is code, not a comment.
    pub fn line_comment(self) -> (&'static str, &'static [&'static str]) {
        match self {
            Language::Python => ("#", &[]),
            Language::Rust => ("//", &["///", "//!"]),
            Language::JavaScript | Language::TypeScript | Language::Tsx => ("//", &["///"]),
        }
    }

    /// Per-function facts for `source` in this language.
    pub fn parse(self, source: &str) -> Result<Vec<FunctionFacts>> {
        match self {
            Language::Python => analyze_file(source),
            Language::JavaScript => js::analyze_js(source, false),
            Language::TypeScript => js::analyze_js(source, true),
            Language::Tsx => js::analyze_tsx(source),
            Language::Rust => rust::analyze_rust(source),
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
    /// Inclusive line span of each top-level body statement, in source order.
    /// The one thing SCIP does not encode (D20): occurrences carry positions but
    /// no statement extents, and a multi-line right-hand side has to group with
    /// the target it defines for def-use to mean anything.
    pub stmt_spans: Vec<(u32, u32)>,
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
    /// α-equivalence: identical when two bodies differ *only* in the names they
    /// bind locally. Literals and free names (callees, imports, globals,
    /// attributes) are preserved, so a differing constant or a different callee
    /// makes two bodies distinct — which is what lets this hash, and only this
    /// hash, justify refusing a write (ADR 0001).
    ///
    /// Sits between the other two: `body_hash` is defeated by any rename,
    /// `structural_hash` erases literals as well as names. Conservative by
    /// design — a name we cannot prove is locally bound stays verbatim, so the
    /// hash under-matches rather than over-matching.
    pub alpha_hash: String,
    /// E-sound equivalence (ADR 0004): α-equivalence modulo rewrite laws true for
    /// every value. Python only; empty for other languages and below the floor.
    pub equiv_hash: String,
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

/// Assignment operators: the token after a name that binds it.
/// Classify a token for the hashes. `local` comes from [`scope::locals`]: the AST, not
/// token adjacency, decides binding, so keyword and attribute names stay free.
fn classify(kind: TokenKind, text: &str, local: bool) -> Tok<'_> {
    match kind {
        TokenKind::Name if local => Tok::Bound(text),
        TokenKind::Name => Tok::Free,
        TokenKind::Int
        | TokenKind::Float
        | TokenKind::Complex
        | TokenKind::String
        | TokenKind::FStringStart
        | TokenKind::FStringMiddle
        | TokenKind::FStringEnd => Tok::Literal,
        _ => Tok::Other,
    }
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
    let mut complexity = 1u32;
    let mut comment_lines = 0u32;
    let mut code_line_set: Vec<u32> = Vec::new();

    let body_tokens: Vec<&Token> = tokens
        .iter()
        .filter(|t| t.range().start() >= body.start() && t.range().end() <= body.end())
        .collect();
    let locals = scope::locals(func);
    let mut hasher = ts_state::HashState::default();
    let mut kind_names: HashMap<TokenKind, String> = HashMap::new();

    // The signature (decorators, defaults, annotations, parameter kinds) is part of
    // what the function means, so it feeds the α-hash; its own name does not.
    let signature_start = func
        .decorator_list
        .iter()
        .map(|decorator| decorator.start())
        .fold(func.start(), TextSize::min);
    let signature = tokens.iter().filter(|t| {
        t.start() >= signature_start && t.end() <= body.start() && t.range() != func.name.range()
    });
    for token in signature {
        let kind = token.kind();
        if is_trivia(kind) || matches!(kind, TokenKind::Comment | TokenKind::Indent | TokenKind::Dedent) {
            continue;
        }
        let text = &source[token.range()];
        let name = kind_names.entry(kind).or_insert_with(|| format!("{kind:?}"));
        hasher.alpha_leaf(name, text, classify(kind, text, locals.is_local(token.start(), text)));
    }

    for token in &body_tokens {
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
        let text = &source[token.range()];
        let name = kind_names.entry(kind).or_insert_with(|| format!("{kind:?}"));
        hasher.leaf(name, text, line, classify(kind, text, locals.is_local(token.start(), text)));
    }
    let significant = hasher.significant;

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

    let (body_hash, structural_hash, alpha_hash) = hasher.finish(MIN_SIGNIFICANT_TOKENS);
    let equiv_hash = if alpha_hash.is_empty() { String::new() } else { equiv::sound_hash(func, source, tokens) };

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
        stmt_spans: func.body.iter().map(|s| (lines.line(s.range().start()), lines.line(s.range().end()))).collect(),
        signature,
        complexity,
        branch_points: cf.branch_points,
        max_nesting_depth: cf.max_depth,
        deepest_line: cf.deepest_line,
        body_hash,
        structural_hash,
        alpha_hash,
        equiv_hash,
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

    /// Bodies must clear the significance floor or every hash is empty and the
    /// comparison passes vacuously.
    fn alpha(src: &str) -> String {
        let f = only(src);
        assert!(!f.alpha_hash.is_empty(), "body is below the significance floor");
        f.alpha_hash
    }

    /// A multi-line right-hand side must stay one statement: if it split, a
    /// callee argument on a continuation line would land in the wrong component.
    #[test]
    fn stmt_spans_group_multi_line_statements() {
        let src = "def f(a):\n    x = 1\n    # noise\n    y = wrap(\n        a,\n        x,\n    )\n    return y\n";
        assert_eq!(only(src).stmt_spans, vec![(1, 1), (3, 6), (7, 7)]);
    }

    /// Written out rather than substring-replaced: a naive replace turned
    /// `FILLER` into `FILLEx` and made the rename test fail for the right reason
    /// on the wrong input.
    const CLEAN: &str = "def clean(rows):\n    out = []\n    for item in rows:\n        if item is not None:\n            out.append(str(item).strip().lower())\n        else:\n            out.append(FILLER)\n    return sorted(out, key=len)\n";
    const CLEAN_RENAMED: &str = "def clean(records):\n    acc = []\n    for entry in records:\n        if entry is not None:\n            acc.append(str(entry).strip().lower())\n        else:\n            acc.append(FILLER)\n    return sorted(acc, key=len)\n";

    /// The property that lets h2 justify a deny: a consistent rename of locals
    /// and parameters is the *same* function.
    #[test]
    fn alpha_hash_is_invariant_under_consistent_rename() {
        let a = only(CLEAN);
        let b = only(CLEAN_RENAMED);
        assert!(!a.alpha_hash.is_empty());
        assert_eq!(a.alpha_hash, b.alpha_hash);
        // Stronger than the shape hash, which also erases literals; weaker than
        // exact text, which any rename defeats.
        assert_ne!(a.body_hash, b.body_hash);
        assert_eq!(a.structural_hash, b.structural_hash);
    }

    /// Why structural_hash cannot be used to deny: it erases constants.
    #[test]
    fn a_different_constant_is_a_different_function() {
        let a = only(&CLEAN.replace("key=len", "key=30"));
        let b = only(&CLEAN.replace("key=len", "key=60"));
        assert!(!a.alpha_hash.is_empty());
        assert_ne!(a.alpha_hash, b.alpha_hash, "30 and 60 are not alpha-equivalent");
        assert_eq!(a.structural_hash, b.structural_hash, "the shape hash erases them");
    }

    /// The catastrophic case for a deny gate: collapsing free names would make
    /// two functions with different callees look identical.
    #[test]
    fn a_different_callee_is_a_different_function() {
        assert_ne!(
            alpha(&CLEAN.replace("strip()", "validate()")),
            alpha(&CLEAN.replace("strip()", "sanitize()")),
            "validate and sanitize are not the same function"
        );
    }

    /// An attribute is named by its owner, so it stays free even when a local
    /// happens to share the name.
    #[test]
    fn an_attribute_name_is_not_a_bound_name() {
        assert_ne!(
            alpha(&CLEAN.replace("str(item)", "item.width")),
            alpha(&CLEAN.replace("str(item)", "item.height")),
            "width and height are different fields"
        );
    }

    /// Under-matching is the safe direction: a global we cannot prove is local
    /// stays verbatim, so two functions reading different globals stay distinct.
    #[test]
    fn an_unprovable_binding_stays_free() {
        assert_ne!(
            alpha(&CLEAN.replace("FILLER", "SCALE")),
            alpha(&CLEAN.replace("FILLER", "OFFSET"))
        );
    }

    #[test]
    fn source_exts_and_from_path_agree() {
        for ext in SOURCE_EXTS {
            assert!(
                Language::from_path(&format!("a.{ext}")).is_some(),
                "SOURCE_EXTS lists {ext} but from_path rejects it"
            );
        }
        assert!(Language::from_path("a.go").is_none());
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

#[cfg(test)]
mod golden {
    /// Pins the hash byte encoding: baselines and sidecars persist these digests,
    /// so an optimisation that changes a value is a breaking change, not a refactor.
    /// Python α changed once, deliberately, for α v2 (AST scopes plus signature).
    #[test]
    fn hash_encoding_is_stable() {
        let py = "def f(a, b):\n    total = a + b * 30\n    for item in a:\n        total += len(str(item))\n    return total\n";
        let js = "function f(a, b) {\n  let total = a + b * 30;\n  for (const item of a) { total += String(item).length; }\n  return total;\n}\n";
        let rs = "fn f(a: &[u32], b: u32) -> u32 {\n    let mut total = b * 30;\n    for item in a { total += item.count_ones(); }\n    total\n}\n";
        let expected = [
            (crate::Language::Python, py, ["6c5fa27bdcbd5f0c548616f3c2170af19fcca37e3794e4950bb9b3461796ac39", "3d24c11c84e45dfffc57ba276a38cc2304d43a49454c0dd8c28591dc8a3c633e", "dfeac5f5c52fe6c0de997879170cac031f8a09e767d560fce0c40ee70ac3ec1b"]),
            (crate::Language::JavaScript, js, ["c5ce10c9afec66d4090a05e9767fbc5202487111cfc6ed4f37c3a79a9e46db82", "bd3f8582380ef18a02dd762b8dc324b22f22fdce915ee25cf62a6200c60bd8e5", "16b0a58367ecd39aa60a93619206c361fca43658c3126310366ccbe74d105b39"]),
            (crate::Language::Rust, rs, ["1df539fa19211957473fa69706a98cc5f060ce6dd1f1d06df6bc3fdd1a6c77b9", "97d02edf3cb5a4b2c77f0036939383d8b1f92452e2a1c625dacd682479ec4386", "ca505f04679b05cb21ee3553f2ba95c86d374ab7f6f973357bd9e81667ccc12e"]),
        ];
        for (language, source, [body, structural, alpha]) in expected {
            let facts = &language.parse(source).unwrap()[0];
            let actual = [facts.body_hash.as_str(), &facts.structural_hash, &facts.alpha_hash];
            assert_eq!(actual, [body, structural, alpha], "{language:?}");
        }
    }
}

/// α v2: each test is a pair a token-adjacency binder got wrong. Unequal pairs
/// would let a deny gate refuse a genuinely different function (ADR 0001).
#[cfg(test)]
mod alpha_soundness {
    fn alpha(src: &str) -> String {
        let facts = crate::analyze_file(src).unwrap();
        assert!(!facts[0].alpha_hash.is_empty(), "below significance floor:\n{src}");
        facts[0].alpha_hash.clone()
    }

    #[test]
    fn keyword_argument_names_are_not_renameable() {
        let f = |kw: &str| alpha(&format!("def f(url, limit):\n    response = get(url, {kw}=limit)\n    response.check()\n    return response.json()\n"));
        assert_ne!(f("timeout"), f("verify"));
    }

    #[test]
    fn a_global_declaration_is_not_a_local_binding() {
        let f = |name: &str| alpha(&format!("def f(xs):\n    global {name}\n    {name} = len(xs) + 1\n    total = {name} * 2 + sum(xs)\n    for item in xs:\n        total += item / 3\n    return total + 7\n"));
        assert_ne!(f("counter"), f("limit"));
    }

    #[test]
    fn defaults_are_part_of_the_function() {
        let f = |value: &str| alpha(&format!("def f(xs, scale={value}):\n    total = 0\n    for x in xs:\n        total += x * scale\n        total -= x / 2\n    return total + len(xs)\n"));
        assert_ne!(f("30"), f("60"));
    }

    #[test]
    fn a_default_reading_a_global_is_not_the_parameter() {
        let f = |name: &str| alpha(&format!("def f({name}={name}):\n    total = {name} * 2\n    for item in range(total):\n        total += item\n    return total + 1\n"));
        assert_ne!(f("left"), f("right"));
    }

    #[test]
    fn comprehension_variables_do_not_leak_into_function_scope() {
        let f = |name: &str| alpha(&format!("def f(xs):\n    ys = [{name} for {name} in xs]\n    total = len(ys) + {name} + 1\n    for item in ys:\n        total += item * 3\n    return total\n"));
        assert_ne!(f("left"), f("right"));
    }

    #[test]
    fn a_bare_import_binds_an_external_name() {
        let f = |module: &str| alpha(&format!("def f(path):\n    import {module}\n    data = {module}.loads(path)\n    return data.get(\"items\", [])\n"));
        assert_ne!(f("json"), f("yaml"));
    }

    #[test]
    fn decorators_are_part_of_the_function() {
        let f = |decorator: &str| alpha(&format!("@{decorator}\ndef f(xs):\n    total = 0\n    for x in xs:\n        total += x * 2\n        total -= x / 3\n    return total + len(xs)\n"));
        assert_ne!(f("cache"), f("retry"));
    }

    /// The property that justifies the hash at all must survive the tightening.
    #[test]
    fn consistent_renames_of_locals_still_match() {
        let f = |a: &str, b: &str, e: &str| alpha(&format!(
            "def f({a}, timeout=30):\n    {b}, rest = split({a})\n    try:\n        out = [x * 2 for x in rest]\n    except ValueError as {e}:\n        log({e})\n        out = []\n    return get({b}, timeout=timeout) + out\n"
        ));
        assert_eq!(f("path", "head", "err"), f("url", "first", "exc"));
    }
}

#[cfg(test)]
mod alpha_comprehensions {
    fn alpha(src: &str) -> String {
        crate::analyze_file(src).unwrap()[0].alpha_hash.clone()
    }

    /// A comprehension-only name refers to its inner binding everywhere: renaming is α.
    #[test]
    fn comprehension_only_names_rename_consistently() {
        let f = |v: &str| alpha(&format!("def f(events, first):\n    ids = {{{v}['id'] for {v} in events if {v}}}\n    assert first['id'] in ids\n    return sorted(ids)[0]\n"));
        assert_eq!(f("e"), f("event"));
    }

    /// The first iterable runs in the enclosing scope: there `e` is a global.
    #[test]
    fn a_first_iterable_reads_the_enclosing_scope() {
        let f = |global: &str| alpha(&format!("def f(xs):\n    total = [e * 2 for e in {global}]\n    for item in xs:\n        total.append(item)\n    return total\n"));
        let shadowing = |global: &str| alpha(&format!("def f(xs):\n    total = [{global} * 2 for {global} in {global}]\n    for item in xs:\n        total.append(item)\n    return total\n"));
        assert_ne!(f("left"), f("right"));
        assert_ne!(shadowing("left"), shadowing("right"));
    }
}
