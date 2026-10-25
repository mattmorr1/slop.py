//! Token/AST facts per function, for the parser-dependent detectors:
//! Tier-1/Tier-2 duplication, purity, branch coverage.
//!
//! Tier-1 hash = token kinds + source text, comments/blank-lines excluded:
//! "exact duplicate modulo comments and whitespace."
//! Tier-2 hash = token kinds only, with names/literals collapsed to
//! placeholders: "same shape, renamed variables / different constants."

use std::collections::HashMap;

use anyhow::{Context, Result};
use ruff_python_ast::token::{Token, TokenKind};
use ruff_python_ast::{self as ast, Stmt};
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
/// detectors (duplication, purity, branch coverage) need a language-aware
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
    /// Structural control-flow decision points only — `if`/`elif`/`for`/
    /// `while`/`except`/`case`, *excluding* boolean operators. This is the
    /// "how many independent branches" signal, uninflated by a single fat
    /// boolean guard (which drives most of the low-end complexity noise).
    pub branch_points: u32,
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

/// Structural decision points in a body: `if`/`elif`/`for`/`while`/`except`/`case`.
/// Boolean operators are not counted; nested `def`/`class` are their own scope.
fn branch_points(stmts: &[Stmt]) -> u32 {
    stmts
        .iter()
        .map(|stmt| match stmt {
            Stmt::If(s) => 1 + branch_points(&s.body) + s.elif_else_clauses.iter().map(|c| u32::from(c.test.is_some()) + branch_points(&c.body)).sum::<u32>(),
            Stmt::For(s) => 1 + branch_points(&s.body) + branch_points(&s.orelse),
            Stmt::While(s) => 1 + branch_points(&s.body) + branch_points(&s.orelse),
            Stmt::Match(s) => s.cases.iter().map(|case| 1 + branch_points(&case.body)).sum(),
            Stmt::Try(s) => {
                let handlers: u32 = s.handlers.iter().map(|ast::ExceptHandler::ExceptHandler(h)| 1 + branch_points(&h.body)).sum();
                branch_points(&s.body) + handlers + branch_points(&s.orelse) + branch_points(&s.finalbody)
            }
            Stmt::With(s) => branch_points(&s.body),
            _ => 0,
        })
        .sum()
}

pub(crate) const MIN_SIGNIFICANT_TOKENS: u32 = 20;

fn function_facts(
    func: &ast::StmtFunctionDef,
    source: &str,
    lines: &LineIndex,
    tokens: &[Token],
) -> FunctionFacts {
    let body = body_range(func);

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
        if is_trivia(kind) || matches!(kind, TokenKind::Indent | TokenKind::Dedent) {
            continue;
        }
        let text = &source[token.range()];
        let name = kind_names.entry(kind).or_insert_with(|| format!("{kind:?}"));
        hasher.leaf(name, text, classify(kind, text, locals.is_local(token.start(), text)));
    }
    let full = func.range();

    let (body_hash, structural_hash, alpha_hash) = hasher.finish(MIN_SIGNIFICANT_TOKENS);
    let equiv_hash = if alpha_hash.is_empty() { String::new() } else { equiv::sound_hash(func, source, tokens) };

    FunctionFacts {
        name: func.name.to_string(),
        name_line: lines.line(func.name.range().start()),
        start_line: lines.line(full.start()),
        end_line: lines.line(full.end()),
        branch_points: branch_points(&func.body),
        body_hash,
        structural_hash,
        alpha_hash,
        equiv_hash,
        decorated: !func.decorator_list.is_empty(),
    }
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
    fn branch_points_ignore_boolean_operators() {
        let flat = analyze_file(
            "def guard(a, b, c, d):\n    if a and b and c and d and a or b:\n        return 1\n    return 0\n",
        )
        .unwrap();
        assert_eq!(flat[0].branch_points, 1, "one structural decision point");

        // Genuinely tangled: three levels of nested branching.
        let tangled = analyze_file(
            "def deep(xs):\n    for x in xs:\n        if x:\n            while x:\n                x -= 1\n    return xs\n",
        )
        .unwrap();
        assert_eq!(tangled[0].branch_points, 3); // for + if + while
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
            (crate::parse::Language::Python, py, ["6c5fa27bdcbd5f0c548616f3c2170af19fcca37e3794e4950bb9b3461796ac39", "3d24c11c84e45dfffc57ba276a38cc2304d43a49454c0dd8c28591dc8a3c633e", "dfeac5f5c52fe6c0de997879170cac031f8a09e767d560fce0c40ee70ac3ec1b"]),
            (crate::parse::Language::JavaScript, js, ["c5ce10c9afec66d4090a05e9767fbc5202487111cfc6ed4f37c3a79a9e46db82", "bd3f8582380ef18a02dd762b8dc324b22f22fdce915ee25cf62a6200c60bd8e5", "16b0a58367ecd39aa60a93619206c361fca43658c3126310366ccbe74d105b39"]),
            (crate::parse::Language::Rust, rs, ["1df539fa19211957473fa69706a98cc5f060ce6dd1f1d06df6bc3fdd1a6c77b9", "97d02edf3cb5a4b2c77f0036939383d8b1f92452e2a1c625dacd682479ec4386", "ca505f04679b05cb21ee3553f2ba95c86d374ab7f6f973357bd9e81667ccc12e"]),
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
        let facts = crate::parse::analyze_file(src).unwrap();
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
        crate::parse::analyze_file(src).unwrap()[0].alpha_hash.clone()
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
