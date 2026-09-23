//! E-equivalence (ADR 0004): α-equivalence modulo a fixed set of rewrite laws.
//!
//! Python functions lower to a small term language. A shared pre-pass applies the
//! syntactic identities, locals get a canonical numbering, then an engine decides
//! equality: [`normalize`] rewrites to a canonical form that can be hashed, and the
//! `egraph` feature checks the same laws by bounded egglog saturation (the ablation).
//!
//! Laws come in two tiers. [`Tier::Sound`] laws hold for every Python value, so a
//! match is an identity a write gate may act on. [`Tier::Graded`] laws hold only for
//! well-behaved types (numeric `+` commutes, `list.__iadd__` does not), so a graded
//! match is advisory, never a deny.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result};
use ruff_python_ast::token::{Token, TokenKind};
use ruff_python_ast::{self as ast, Expr, Number, Stmt, UnaryOp};
use ruff_python_parser::parse_module;
use ruff_text_size::{Ranged, TextRange};

use crate::scope::{self, Locals};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Term {
    /// A function-local binding: a source name before numbering, `#k` after.
    Local(String),
    /// A free name: callee, global, import. Never renamed.
    Name(String),
    Lit(String),
    Node(String, Vec<Term>),
    Block(Vec<Term>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    Sound,
    Graded,
}

fn node(label: impl Into<String>, children: Vec<Term>) -> Term {
    Term::Node(label.into(), children)
}

fn none() -> Term {
    Term::Lit("none".into())
}

fn short_hash(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex()[..16].to_string()
}

pub struct EquivFacts {
    pub name: String,
    pub line: u32,
    /// Canonical term after the pre-pass and local numbering, before any engine law.
    pub term: Term,
    pub sound: String,
    pub graded: String,
}

/// Every top-level and nested `def` in `source`, with both tiers' canonical hashes.
pub fn equivalence_facts(source: &str) -> Result<Vec<EquivFacts>> {
    let parsed = parse_module(source).context("parsing python module")?;
    let tokens = parsed.tokens();
    let lines = crate::LineIndex::new(source);
    let mut facts = Vec::new();
    crate::collect_functions(parsed.syntax().body.as_slice(), &mut |func| {
        let term = lower_function(func, source, tokens.as_ref());
        facts.push(EquivFacts {
            name: func.name.to_string(),
            line: lines.line(func.name.start()),
            sound: term_hash(&normalize(term.clone(), Tier::Sound)),
            graded: term_hash(&normalize(term.clone(), Tier::Graded)),
            term,
        });
    });
    Ok(facts)
}

pub fn term_hash(term: &Term) -> String {
    let mut hasher = blake3::Hasher::new();
    feed(&mut hasher, term);
    hasher.finalize().to_hex().to_string()
}

fn feed(hasher: &mut blake3::Hasher, term: &Term) {
    let (tag, text, children): (&[u8], &str, &[Term]) = match term {
        Term::Local(name) => (b"L", name, &[]),
        Term::Name(name) => (b"N", name, &[]),
        Term::Lit(value) => (b"C", value, &[]),
        Term::Node(label, children) => (b"(", label, children),
        Term::Block(children) => (b"{", "", children),
    };
    hasher.update(tag).update(text.as_bytes()).update(b"\x00");
    children.iter().for_each(|child| feed(hasher, child));
    hasher.update(b")");
}

// ---------------------------------------------------------------- lowering

struct Lower<'a> {
    source: &'a str,
    tokens: &'a [Token],
    locals: Locals,
}

impl Lower<'_> {
    fn block(&self, stmts: &[Stmt]) -> Term {
        Term::Block(stmts.iter().filter_map(|stmt| self.stmt(stmt)).collect())
    }

    fn stmt(&self, stmt: &Stmt) -> Option<Term> {
        Some(match stmt {
            Stmt::Pass(_) => return None,
            Stmt::Return(ret) => node("return", vec![ret.value.as_deref().map_or_else(none, |value| self.expr(value))]),
            Stmt::Expr(expr) => node("expr", vec![self.expr(&expr.value)]),
            Stmt::Assign(assign) => node(
                "assign",
                assign.targets.iter().chain([&*assign.value]).map(|expr| self.expr(expr)).collect(),
            ),
            Stmt::AugAssign(aug) => node(
                format!("aug:{}", aug.op.as_str()),
                vec![self.expr(&aug.target), self.expr(&aug.value)],
            ),
            Stmt::If(branch) => self.if_chain(&branch.test, &branch.body, &branch.elif_else_clauses),
            Stmt::For(for_) if !for_.is_async => node(
                "for",
                vec![self.expr(&for_.target), self.expr(&for_.iter), self.block(&for_.body), self.block(&for_.orelse)],
            ),
            Stmt::While(while_) => node(
                "while",
                vec![self.expr(&while_.test), self.block(&while_.body), self.block(&while_.orelse)],
            ),
            Stmt::Raise(raise) => node(
                "raise",
                [&raise.exc, &raise.cause]
                    .into_iter()
                    .map(|expr| expr.as_deref().map_or_else(none, |expr| self.expr(expr)))
                    .collect(),
            ),
            Stmt::Break(_) => node("break", Vec::new()),
            Stmt::Continue(_) => node("continue", Vec::new()),
            Stmt::For(for_) => node(
                "afor",
                vec![self.expr(&for_.target), self.expr(&for_.iter), self.block(&for_.body), self.block(&for_.orelse)],
            ),
            Stmt::With(with) => node(
                if with.is_async { "awith" } else { "with" },
                with.items
                    .iter()
                    .map(|item| {
                        let target = item.optional_vars.as_deref().map_or_else(none, |vars| self.expr(vars));
                        node("item", vec![self.expr(&item.context_expr), target])
                    })
                    .chain([self.block(&with.body)])
                    .collect(),
            ),
            Stmt::Try(try_) => node(
                if try_.is_star { "try*" } else { "try" },
                std::iter::once(self.block(&try_.body))
                    .chain(try_.handlers.iter().map(|handler| {
                        let ast::ExceptHandler::ExceptHandler(handler) = handler;
                        let kind = handler.type_.as_deref().map_or_else(none, |kind| self.expr(kind));
                        let name = handler.name.as_ref().map_or_else(none, |name| {
                            if self.locals.is_local(name.start(), name.as_str()) {
                                Term::Local(name.to_string())
                            } else {
                                Term::Name(name.to_string())
                            }
                        });
                        node("handler", vec![kind, name, self.block(&handler.body)])
                    }))
                    .chain([self.block(&try_.orelse), self.block(&try_.finalbody)])
                    .collect(),
            ),
            Stmt::Assert(assert) => node(
                "assert",
                vec![self.expr(&assert.test), assert.msg.as_deref().map_or_else(none, |msg| self.expr(msg))],
            ),
            Stmt::Delete(delete) => node("del", delete.targets.iter().map(|target| self.expr(target)).collect()),
            Stmt::AnnAssign(ann) => node(
                "annassign",
                vec![
                    self.expr(&ann.target),
                    self.expr(&ann.annotation),
                    ann.value.as_deref().map_or_else(none, |value| self.expr(value)),
                ],
            ),
            _ => self.opaque("stmt", stmt.range()),
        })
    }

    fn if_chain(&self, test: &Expr, body: &[Stmt], clauses: &[ast::ElifElseClause]) -> Term {
        let orelse = match clauses.split_first() {
            None => Term::Block(Vec::new()),
            Some((clause, rest)) => match &clause.test {
                Some(test) => Term::Block(vec![self.if_chain(test, &clause.body, rest)]),
                None => self.block(&clause.body),
            },
        };
        node("if", vec![self.expr(test), self.block(body), orelse])
    }

    fn expr(&self, expr: &Expr) -> Term {
        match expr {
            Expr::Name(name) if self.locals.is_local(name.start(), name.id.as_str()) => {
                Term::Local(name.id.to_string())
            }
            Expr::Name(name) => Term::Name(name.id.to_string()),
            Expr::NumberLiteral(number) => Term::Lit(match &number.value {
                Number::Int(int) => format!("int:{int}"),
                Number::Float(float) => format!("float:{}", float.to_bits()),
                Number::Complex { real, imag } => format!("complex:{}:{}", real.to_bits(), imag.to_bits()),
            }),
            Expr::StringLiteral(string) => Term::Lit(format!("str:{}", short_hash(string.value.to_str()))),
            Expr::BooleanLiteral(boolean) => Term::Lit(format!("bool:{}", boolean.value)),
            Expr::NoneLiteral(_) => none(),
            Expr::BinOp(bin) => node(bin.op.as_str(), vec![self.expr(&bin.left), self.expr(&bin.right)]),
            Expr::UnaryOp(unary) if unary.op == UnaryOp::Not => node("not", vec![self.expr(&unary.operand)]),
            Expr::UnaryOp(unary) => node(format!("unary:{}", unary.op.as_str()), vec![self.expr(&unary.operand)]),
            // Right-nested binary form, so both engines see the same shape.
            Expr::BoolOp(bool_op) => bool_op
                .values
                .iter()
                .rev()
                .map(|value| self.expr(value))
                .reduce(|rest, value| node(bool_op.op.as_str(), vec![value, rest]))
                .unwrap_or_else(none),
            Expr::Compare(compare) if compare.ops.len() == 1 => node(
                compare.ops[0].as_str(),
                vec![self.expr(&compare.left), self.expr(&compare.comparators[0])],
            ),
            Expr::Compare(compare) => node(
                "cmpchain",
                std::iter::once(self.expr(&compare.left))
                    .chain(compare.ops.iter().zip(compare.comparators.iter()).flat_map(|(op, right)| {
                        [Term::Lit(format!("op:{}", op.as_str())), self.expr(right)]
                    }))
                    .collect(),
            ),
            Expr::Call(call) => node(
                "call",
                std::iter::once(self.expr(&call.func))
                    .chain(call.arguments.args.iter().map(|arg| self.expr(arg)))
                    .chain(call.arguments.keywords.iter().map(|keyword| match &keyword.arg {
                        Some(name) => node(format!("kw:{name}"), vec![self.expr(&keyword.value)]),
                        None => node("kw**", vec![self.expr(&keyword.value)]),
                    }))
                    .collect(),
            ),
            Expr::Attribute(attr) => node(format!("attr:{}", attr.attr.as_str()), vec![self.expr(&attr.value)]),
            Expr::If(if_) => node("ifexp", vec![self.expr(&if_.test), self.expr(&if_.body), self.expr(&if_.orelse)]),
            Expr::Subscript(sub) => node("sub", vec![self.expr(&sub.value), self.expr(&sub.slice)]),
            Expr::Tuple(tuple) => node("tuple", tuple.elts.iter().map(|elt| self.expr(elt)).collect()),
            Expr::List(list) => node("list", list.elts.iter().map(|elt| self.expr(elt)).collect()),
            Expr::Starred(star) => node("star", vec![self.expr(&star.value)]),
            Expr::Set(set) => node("set", set.elts.iter().map(|elt| self.expr(elt)).collect()),
            Expr::Dict(dict) => node(
                "dict",
                dict.items
                    .iter()
                    .map(|item| match &item.key {
                        Some(key) => node("pair", vec![self.expr(key), self.expr(&item.value)]),
                        None => node("unpack", vec![self.expr(&item.value)]),
                    })
                    .collect(),
            ),
            Expr::Slice(slice) => node(
                "slice",
                [&slice.lower, &slice.upper, &slice.step]
                    .into_iter()
                    .map(|part| part.as_deref().map_or_else(none, |part| self.expr(part)))
                    .collect(),
            ),
            Expr::Await(await_) => node("await", vec![self.expr(&await_.value)]),
            Expr::Yield(yield_) => node("yield", vec![yield_.value.as_deref().map_or_else(none, |v| self.expr(v))]),
            Expr::YieldFrom(yield_from) => node("yieldfrom", vec![self.expr(&yield_from.value)]),
            Expr::Named(named) => node("walrus", vec![self.expr(&named.target), self.expr(&named.value)]),
            Expr::EllipsisLiteral(_) => Term::Lit("ellipsis".into()),
            Expr::ListComp(comp) => self.comprehension("listcomp", &[&comp.elt], &comp.generators),
            Expr::SetComp(comp) => self.comprehension("setcomp", &[&comp.elt], &comp.generators),
            Expr::Generator(comp) => self.comprehension("genexp", &[&comp.elt], &comp.generators),
            Expr::DictComp(comp) => {
                let key = comp.key.as_deref().map_or_else(none, |key| self.expr(key));
                let mut term = self.comprehension("dictcomp", &[&comp.value], &comp.generators);
                if let Term::Node(_, children) = &mut term {
                    children.insert(0, key);
                }
                term
            }
            Expr::Lambda(lambda) => {
                let params = lambda.parameters.iter().flat_map(|parameters| {
                    let named = parameters.posonlyargs.iter().chain(&parameters.args).chain(&parameters.kwonlyargs);
                    named
                        .map(|p| (&p.parameter, p.default.as_deref()))
                        .chain([&parameters.vararg, &parameters.kwarg].into_iter().flatten().map(|p| (&**p, None)))
                });
                node(
                    "lambda",
                    params
                        .map(|(parameter, default)| {
                            let name = if self.locals.is_local(parameter.name.start(), parameter.name.as_str()) {
                                Term::Local(parameter.name.to_string())
                            } else {
                                Term::Name(parameter.name.to_string())
                            };
                            node("param", vec![name, default.map_or_else(none, |d| self.expr(d))])
                        })
                        .chain([self.expr(&lambda.body)])
                        .collect(),
                )
            }
            _ => self.opaque("expr", expr.range()),
        }
    }

    fn comprehension(&self, label: &str, elts: &[&Expr], generators: &[ast::Comprehension]) -> Term {
        node(
            label,
            elts.iter()
                .map(|elt| self.expr(elt))
                .chain(generators.iter().map(|generator| {
                    let head = [self.expr(&generator.target), self.expr(&generator.iter)];
                    let label = if generator.is_async { "agen" } else { "gen" };
                    node(label, head.into_iter().chain(generator.ifs.iter().map(|test| self.expr(test))).collect())
                }))
                .collect(),
        )
    }

    /// A construct the term language does not model: its α-classified token stream.
    /// Equal only when identical, so falling back here costs recall, never soundness.
    fn opaque(&self, kind: &str, range: TextRange) -> Term {
        let from = self.tokens.partition_point(|token| token.start() < range.start());
        let children = self.tokens[from..]
            .iter()
            .take_while(|token| token.end() <= range.end())
            .filter(|token| !matches!(token.kind(), TokenKind::Comment | TokenKind::NonLogicalNewline))
            .map(|token| {
                let text = &self.source[token.range()];
                match token.kind() {
                    TokenKind::Name if self.locals.is_local(token.start(), text) => Term::Local(text.into()),
                    TokenKind::Name => Term::Name(text.into()),
                    kind => Term::Lit(format!("{kind:?}:{}", short_hash(text))),
                }
            })
            .collect();
        node(format!("opaque:{kind}"), children)
    }
}

fn lower_function(func: &ast::StmtFunctionDef, source: &str, tokens: &[Token]) -> Term {
    let lower = Lower {
        source,
        tokens,
        locals: scope::locals(func),
    };
    let parameters = &func.parameters;
    let params: Vec<String> = parameters
        .posonlyargs
        .iter()
        .chain(&parameters.args)
        .map(|parameter| parameter.parameter.name.to_string())
        .chain(parameters.vararg.iter().map(|parameter| parameter.name.to_string()))
        .chain(parameters.kwonlyargs.iter().map(|parameter| parameter.parameter.name.to_string()))
        .chain(parameters.kwarg.iter().map(|parameter| parameter.name.to_string()))
        .collect();
    // Defaults and parameter kinds are part of the function; names are not.
    let signature = node(
        "sig",
        [&parameters.posonlyargs, &parameters.args, &parameters.kwonlyargs]
            .into_iter()
            .enumerate()
            .flat_map(|(kind, group)| {
                let lower = &lower;
                group.iter().map(move |parameter| {
                    let default = parameter.default.as_deref().map_or_else(|| Term::Lit("nodefault".into()), |d| lower.expr(d));
                    node(format!("param:{kind}"), vec![default])
                })
            })
            .chain(parameters.vararg.iter().map(|_| node("param:*", Vec::new())))
            .chain(parameters.kwarg.iter().map(|_| node("param:**", Vec::new())))
            .chain(func.decorator_list.iter().map(|decorator| node("decorator", vec![lower.expr(&decorator.expression)])))
            .collect(),
    );
    let mut body = lower.block(&func.body);
    if !terminates(&body) {
        // Falling off the end returns None.
        if let Term::Block(stmts) = &mut body {
            stmts.push(node("return", vec![none()]));
        }
    }
    let body = inline_temps(body, &params);
    number_locals(fold_ints(node("fn", vec![signature, body])), &params)
}

// ---------------------------------------------------------------- pre-pass

fn count_local(term: &Term, name: &str) -> usize {
    match term {
        Term::Local(local) => usize::from(local == name),
        Term::Node(_, children) | Term::Block(children) => children.iter().map(|child| count_local(child, name)).sum(),
        _ => 0,
    }
}

/// `x = e; return x` becomes `return e` when that is x's only definition and only
/// use anywhere in the function; a closure reading x later would otherwise see it.
fn inline_temps(body: Term, params: &[String]) -> Term {
    let whole = body.clone();
    map_blocks(body, &mut |stmts| {
        let mut out: Vec<Term> = Vec::with_capacity(stmts.len());
        for stmt in stmts {
            let inlined = match (out.last(), &stmt) {
                (Some(Term::Node(assign, pair)), Term::Node(ret, value))
                    if assign == "assign" && ret == "return" && pair.len() == 2 =>
                {
                    match (&pair[0], value.as_slice()) {
                        (Term::Local(x), [Term::Local(y)])
                            if x == y && !params.contains(x) && count_local(&whole, x) == 2 =>
                        {
                            Some(node("return", vec![pair[1].clone()]))
                        }
                        _ => None,
                    }
                }
                _ => None,
            };
            match inlined {
                Some(ret) => {
                    out.pop();
                    out.push(ret);
                }
                None => out.push(stmt),
            }
        }
        out
    })
}

fn map_blocks(term: Term, f: &mut impl FnMut(Vec<Term>) -> Vec<Term>) -> Term {
    match term {
        Term::Block(stmts) => {
            let stmts = stmts.into_iter().map(|stmt| map_blocks(stmt, f)).collect();
            Term::Block(f(stmts))
        }
        Term::Node(label, children) => {
            Term::Node(label, children.into_iter().map(|child| map_blocks(child, f)).collect())
        }
        leaf => leaf,
    }
}

fn int_value(term: &Term) -> Option<i64> {
    match term {
        Term::Lit(value) => value.strip_prefix("int:")?.parse().ok(),
        _ => None,
    }
}

/// Integer arithmetic on literals is exact in Python, so folding it is sound.
fn fold_ints(term: Term) -> Term {
    match term {
        Term::Node(label, children) => {
            let children: Vec<Term> = children.into_iter().map(fold_ints).collect();
            let folded = match (label.as_str(), children.as_slice()) {
                ("+", [a, b]) => int_value(a).zip(int_value(b)).and_then(|(a, b)| a.checked_add(b)),
                ("-", [a, b]) => int_value(a).zip(int_value(b)).and_then(|(a, b)| a.checked_sub(b)),
                ("*", [a, b]) => int_value(a).zip(int_value(b)).and_then(|(a, b)| a.checked_mul(b)),
                ("unary:-", [a]) => int_value(a).and_then(i64::checked_neg),
                _ => None,
            };
            folded.map_or(Term::Node(label, children), |value| Term::Lit(format!("int:{value}")))
        }
        Term::Block(stmts) => Term::Block(stmts.into_iter().map(fold_ints).collect()),
        leaf => leaf,
    }
}

// ---------------------------------------------------------------- numbering

fn mask(term: &Term) -> Term {
    match term {
        Term::Local(name) if !name.starts_with('#') => Term::Local("_".into()),
        Term::Node(label, children) => Term::Node(label.clone(), children.iter().map(mask).collect()),
        Term::Block(children) => Term::Block(children.iter().map(mask).collect()),
        leaf => leaf.clone(),
    }
}

fn first_seen(term: &Term, order: &mut Vec<String>) {
    match term {
        Term::Local(name) if !order.contains(name) => order.push(name.clone()),
        Term::Node(_, children) | Term::Block(children) => children.iter().for_each(|child| first_seen(child, order)),
        _ => {}
    }
}

/// The masked right-hand sides of every plain assignment to each local, sorted: a
/// key the control-flow laws cannot change, unlike source position or branch order.
fn definition_keys(term: &Term, keys: &mut BTreeMap<String, Vec<Term>>) {
    if let Term::Node(label, children) = term {
        if label == "assign" && children.len() == 2 {
            if let Term::Local(name) = &children[0] {
                keys.entry(name.clone()).or_default().push(mask(&children[1]));
            }
        }
    }
    if let Term::Node(_, children) | Term::Block(children) = term {
        children.iter().for_each(|child| definition_keys(child, keys));
    }
}

fn rename(term: Term, map: &HashMap<String, String>) -> Term {
    match term {
        Term::Local(name) => Term::Local(map.get(&name).cloned().unwrap_or(name)),
        Term::Node(label, children) => Term::Node(label, children.into_iter().map(|child| rename(child, map)).collect()),
        Term::Block(children) => Term::Block(children.into_iter().map(|child| rename(child, map)).collect()),
        leaf => leaf,
    }
}

/// Parameters by position, then other locals by definition key (ties by first
/// appearance). Any consistent renaming under which two terms are equal is itself
/// a valid α-renaming, so an unlucky tie can only cost recall.
fn number_locals(term: Term, params: &[String]) -> Term {
    let mut map: HashMap<String, String> =
        params.iter().enumerate().map(|(index, name)| (name.clone(), format!("#{index}"))).collect();
    let term = rename(term, &map);
    let mut order = Vec::new();
    first_seen(&term, &mut order);
    let mut keys = BTreeMap::new();
    definition_keys(&term, &mut keys);
    keys.values_mut().for_each(|definitions| definitions.sort());
    let mut rest: Vec<(Option<Vec<Term>>, usize, String)> = order
        .into_iter()
        .enumerate()
        .filter(|(_, name)| !name.starts_with('#'))
        .map(|(seen, name)| (keys.get(&name).cloned(), seen, name))
        .collect();
    rest.sort();
    let offset = map.len();
    map = rest
        .into_iter()
        .enumerate()
        .map(|(index, (_, _, name))| (name, format!("#{}", offset + index)))
        .collect();
    rename(term, &map)
}

// ---------------------------------------------------------------- normalizer

fn is_exit(term: &Term) -> bool {
    matches!(term, Term::Node(label, _) if matches!(label.as_str(), "return" | "raise" | "break" | "continue"))
}

/// Control never falls out of this block: it ends in an exit, or in an `if`
/// whose two branches both never fall out.
fn terminates(block: &Term) -> bool {
    match block {
        Term::Block(stmts) => stmts.last().is_some_and(|last| {
            is_exit(last) || matches!(last, Term::Node(label, c) if label == "if" && terminates(&c[1]) && terminates(&c[2]))
        }),
        _ => false,
    }
}

fn single_return(block: &Term) -> Option<&Term> {
    match block {
        Term::Block(stmts) => match stmts.as_slice() {
            [Term::Node(label, value)] if label == "return" => value.first(),
            _ => None,
        },
        _ => None,
    }
}

const COMMUTATIVE: &[&str] = &["+", "*", "==", "!=", "&", "|", "^"];

/// Rewrite to the tier's canonical form: bottom-up, each node to a local fixpoint.
/// Every rule shrinks the term or moves it toward a fixed orientation, so this
/// terminates; the rule set is confluent by construction of those orientations.
pub fn normalize(term: Term, tier: Tier) -> Term {
    let term = match term {
        Term::Node(label, children) => {
            Term::Node(label, children.into_iter().map(|child| normalize(child, tier)).collect())
        }
        Term::Block(stmts) => normalize_block(stmts.into_iter().map(|stmt| normalize(stmt, tier)).collect(), tier),
        leaf => return leaf,
    };
    let mut current = term;
    loop {
        match rewrite(&current, tier) {
            Some(next) => current = normalize(next, tier),
            None => return current,
        }
    }
}

fn normalize_block(stmts: Vec<Term>, tier: Tier) -> Term {
    let mut out: Vec<Term> = Vec::with_capacity(stmts.len());
    let mut stmts = stmts.into_iter();
    while let Some(stmt) = stmts.next() {
        // Code after an unconditional exit is unreachable.
        if is_exit(&stmt) {
            out.push(stmt);
            break;
        }
        // `if c: <exits> ; rest` is `if c: <exits> else: rest` (and symmetrically).
        if let Term::Node(label, children) = &stmt {
            if label == "if" && stmts.len() > 0 && (terminates(&children[1]) || terminates(&children[2])) {
                let rest: Vec<Term> = stmts.by_ref().collect();
                let [test, then, orelse] = <[Term; 3]>::try_from(children.clone()).expect("if has three children");
                let append = |block: Term| match block {
                    Term::Block(mut inner) => {
                        inner.extend(rest.iter().cloned());
                        Term::Block(inner)
                    }
                    other => other,
                };
                let nested = if terminates(&then) {
                    node("if", vec![test, then, append(orelse)])
                } else {
                    node("if", vec![test, append(then), orelse])
                };
                out.push(normalize(nested, tier));
                break;
            }
        }
        out.push(stmt);
    }
    Term::Block(out)
}

fn rewrite(term: &Term, tier: Tier) -> Option<Term> {
    let Term::Node(label, children) = term else { return None };
    match (label.as_str(), children.as_slice()) {
        // `if not c: A else: B` is `if c: B else: A`: one __bool__ call either way.
        ("if" | "ifexp", [Term::Node(not, inner), a, b]) if not == "not" && inner.len() == 1 => {
            Some(node(label.clone(), vec![inner[0].clone(), b.clone(), a.clone()]))
        }
        ("if", [test, then, orelse]) => {
            let (a, b) = (single_return(then)?, single_return(orelse)?);
            Some(node("return", vec![node("ifexp", vec![test.clone(), a.clone(), b.clone()])]))
        }
        _ if tier == Tier::Sound => None,
        (">", [a, b]) => Some(node("<", vec![b.clone(), a.clone()])),
        (">=", [a, b]) => Some(node("<=", vec![b.clone(), a.clone()])),
        ("not", [Term::Node(inner, operands)]) if inner == "not" && operands.len() == 1 => Some(operands[0].clone()),
        ("not", [Term::Node(inner, operands)]) if (inner == "and" || inner == "or") && operands.len() == 2 => {
            let dual = if inner == "and" { "or" } else { "and" };
            Some(node(dual, operands.iter().map(|operand| node("not", vec![operand.clone()])).collect()))
        }
        (aug, [target, value]) if aug.starts_with("aug:") => Some(node(
            "assign",
            vec![target.clone(), node(&aug[4..], vec![target.clone(), value.clone()])],
        )),
        (op, _) if COMMUTATIVE.contains(&op) && children.len() >= 2 => {
            // Flatten associative chains, then sort: AC-canonical form.
            let mut flat: Vec<Term> = children
                .iter()
                .flat_map(|child| match child {
                    Term::Node(inner, grand) if inner == op && (op == "+" || op == "*") => grand.clone(),
                    other => vec![other.clone()],
                })
                .collect();
            flat.sort();
            (flat != *children).then(|| node(op, flat))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hashes(src: &str) -> (String, String) {
        let facts = equivalence_facts(src).unwrap();
        (facts[0].sound.clone(), facts[0].graded.clone())
    }

    fn sound(src: &str) -> String {
        hashes(src).0
    }

    #[test]
    fn ternary_and_branches_are_sound_equal() {
        let a = "def f(x, c):\n    if c:\n        return x + 1\n    else:\n        return x - 1\n";
        let b = "def f(y, flag):\n    return y + 1 if flag else y - 1\n";
        let c = "def f(x, c):\n    if c:\n        return x + 1\n    return x - 1\n";
        assert_eq!(sound(a), sound(b));
        assert_eq!(sound(a), sound(c));
    }

    #[test]
    fn negated_test_with_swapped_branches_is_sound_equal() {
        let a = "def f(x):\n    if x > 0:\n        y = g(x)\n    else:\n        y = h(x)\n    return y\n";
        let b = "def f(x):\n    if not x > 0:\n        z = h(x)\n    else:\n        z = g(x)\n    return z\n";
        assert_eq!(sound(a), sound(b));
    }

    #[test]
    fn temp_inline_dead_code_and_int_folding_are_sound() {
        let a = "def f(x):\n    result = x * 60\n    return result\n    print(x)\n";
        let b = "def f(x):\n    return x * (2 * 30)\n";
        assert_eq!(sound(a), sound(b));
    }

    #[test]
    fn implicit_return_none_is_sound_equal() {
        let a = "def f(x):\n    if x:\n        return 1\n";
        let b = "def f(x):\n    return 1 if x else None\n";
        assert_eq!(sound(a), sound(b));
    }

    /// Each of these is a real behaviour change; the sound tier must keep them apart.
    #[test]
    fn traps_are_not_sound_equal() {
        let base = "def f(xs, ys):\n    xs = xs + ys\n    return xs\n";
        let aug = "def f(xs, ys):\n    xs += ys\n    return xs\n";
        assert_ne!(sound(base), sound(aug), "list += mutates in place");
        let add = "def f(a, b):\n    return a + b\n";
        let swapped = "def f(a, b):\n    return b + a\n";
        assert_ne!(sound(add), sound(swapped), "str + does not commute");
        let morgan = "def f(a, b):\n    return not (a and b)\n";
        let dual = "def f(a, b):\n    return not a or not b\n";
        assert_ne!(sound(morgan), sound(dual), "__bool__ call counts differ");
        let negated = "def f(x):\n    if not x:\n        return 1\n    return 2\n";
        let plain = "def f(x):\n    if x:\n        return 1\n    return 2\n";
        assert_ne!(sound(negated), sound(plain));
    }

    #[test]
    fn graded_tier_equates_algebra_the_sound_tier_refuses() {
        let (add_sound, add_graded) = hashes("def f(a, b):\n    return a + b\n");
        let (swap_sound, swap_graded) = hashes("def f(a, b):\n    return b + a\n");
        assert_ne!(add_sound, swap_sound);
        assert_eq!(add_graded, swap_graded);
    }
}
