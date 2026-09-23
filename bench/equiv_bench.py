#!/usr/bin/env python3
"""B3: the equivalence ladder, measured on mutants of real functions.

Ground truth comes from construction, not labelling. Each real Python function
is paired with a mutant:

- P (preserving): a rewrite provably equivalent for *every* Python value
  (consistent local rename, ternary <-> branches, negate-and-swap, dead code,
  temp inline, implicit `return None`, int folding, `pass`, and combinations).
- B (breaking): not provably equivalent. `real` mutants change behaviour
  (constant, callee, keyword, comparison, statement order, default). `trap`
  mutants are laws that hold only for well-behaved types (`x += y`, `a + b ->
  b + a`, De Morgan, comparison flips): they are exactly why a graded match may
  never deny a write.

A tier that says "equal" on any B pair has made a false-equivalence claim. The
sound tier must score 0 there; recall on P is what it buys.

Arms: exact (body_hash), structural, alpha v1 (pinned commit, old token
binder), alpha v2, E-sound and E-graded via the normalizer, the same via egglog
saturation, and optionally jscpd (an industry token clone detector).

Usage:
  cargo build --release -p slop-cli --features egraph
  python3 bench/equiv_bench.py [--repos DIR ...] [--limit N] [--seed S] [--jscpd]
"""
from __future__ import annotations

import argparse
import ast
import copy
import hashlib
import json
import math
import os
import platform
import random
import statistics
import subprocess
import sys
import symtable
import tempfile
import textwrap
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SLOP = ROOT / "target/release/slop"
ALPHA_V1_COMMIT = "8b0d6b1"  # last commit with the token-adjacency Python binder
SKIP_DIRS = {".git", "node_modules", ".venv", "venv", "target", "__pycache__", "build",
             "dist", "htmlcov", "coverage", ".next", ".nuxt", ".svelte-kit", ".slop"}
DEFAULT_REPOS = [Path.home() / "Documents/GitHub" / name
                 for name in ("stress-analysis", "geoguessrbot", "vigil")]


# ------------------------------------------------------------------ corpus

def functions(repos: list[Path]):
    for repo in repos:
        found = []
        for directory, subdirs, files in os.walk(repo):
            subdirs[:] = sorted(d for d in subdirs if d not in SKIP_DIRS)
            found += [Path(directory) / f for f in files if f.endswith(".py")]
        for path in sorted(found):
            try:
                text = path.read_text()
                tree = ast.parse(text)
            except (SyntaxError, UnicodeDecodeError, ValueError):
                continue
            for node in ast.walk(tree):
                if isinstance(node, ast.FunctionDef) and 3 <= len(node.body) and node.end_lineno - node.lineno <= 60:
                    source = textwrap.dedent(ast.get_source_segment(text, node) or "")
                    try:
                        fn = ast.parse(source).body[0]
                    except SyntaxError:
                        continue
                    yield f"{repo.name}/{path.relative_to(repo)}:{node.lineno}", fn


def unparse(fn: ast.FunctionDef) -> str:
    return ast.unparse(ast.fix_missing_locations(fn)) + "\n"


def identifiers(fn: ast.AST) -> set[str]:
    return {n.id for n in ast.walk(fn) if isinstance(n, ast.Name)} | {
        n.arg for n in ast.walk(fn) if isinstance(n, ast.arg)}


def fresh(fn: ast.AST, stem: str) -> str:
    taken = identifiers(fn)
    return next(f"{stem}{i}" for i in range(10_000) if f"{stem}{i}" not in taken)


def blocks(fn: ast.AST):
    """Every statement list in the function, including nested ones."""
    for node in ast.walk(fn):
        for field in ("body", "orelse", "finalbody"):
            value = getattr(node, field, None)
            if isinstance(value, list) and value and isinstance(value[0], ast.stmt):
                yield value


def int_constants(fn: ast.AST):
    return [n for n in ast.walk(fn) if isinstance(n, ast.Constant) and type(n.value) is int]


# ------------------------------------------------------------------ preserving

def p_rename(fn, rng):
    source = unparse(fn)
    table = symtable.symtable(source, "<fn>", "exec").get_children()[0]
    nested = {name for child in table.get_children() for name in child.get_identifiers()}
    locals_ = [s.get_name() for s in table.get_symbols()
               if (s.is_local() or s.is_parameter()) and not (s.is_global() or s.is_nonlocal()
                   or s.is_imported() or s.is_namespace() or s.is_declared_global())
               and s.get_name() not in nested]
    if not locals_:
        return None
    taken = identifiers(fn)
    mapping, counter = {}, 0
    for name in locals_:
        while f"v{counter}" in taken:
            counter += 1
        mapping[name] = f"v{counter}"
        counter += 1

    class Rename(ast.NodeTransformer):
        def visit_Name(self, node):
            node.id = mapping.get(node.id, node.id)
            return node

        def visit_arg(self, node):
            node.arg = mapping.get(node.arg, node.arg)
            return self.generic_visit(node)

        def visit_ExceptHandler(self, node):
            node.name = mapping.get(node.name, node.name) if node.name else None
            return self.generic_visit(node)

    fn = Rename().visit(fn)
    fn.name = fn.name  # the function's own name is not part of the body
    return fn


def p_ternary(fn, rng):
    sites = []
    for block in blocks(fn):
        for i, stmt in enumerate(block):
            if (isinstance(stmt, ast.If) and len(stmt.body) == 1 and isinstance(stmt.body[0], ast.Return)
                    and stmt.body[0].value is not None):
                if len(stmt.orelse) == 1 and isinstance(stmt.orelse[0], ast.Return) and stmt.orelse[0].value is not None:
                    sites.append((block, i, stmt.orelse[0].value, 1))
                elif not stmt.orelse and i + 1 < len(block) and isinstance(block[i + 1], ast.Return) \
                        and block[i + 1].value is not None:
                    sites.append((block, i, block[i + 1].value, 2))
    if not sites:
        return None
    block, i, other, width = rng.choice(sites)
    stmt = block[i]
    block[i:i + width] = [ast.Return(ast.IfExp(stmt.test, stmt.body[0].value, other))]
    return fn


def p_negate(fn, rng):
    sites = [n for n in ast.walk(fn) if isinstance(n, ast.If) and n.orelse]
    if not sites:
        return None
    node = rng.choice(sites)
    test = node.test.operand if isinstance(node.test, ast.UnaryOp) and isinstance(node.test.op, ast.Not) \
        else ast.UnaryOp(ast.Not(), node.test)
    node.test, node.body, node.orelse = test, node.orelse, node.body
    return fn


def p_dead(fn, rng):
    if not isinstance(fn.body[-1], (ast.Return, ast.Raise)):
        return None
    fn.body.append(ast.Expr(ast.Call(ast.Name("print"), [ast.Constant("unreachable")], [])))
    return fn


def p_temp(fn, rng):
    last = fn.body[-1]
    if not isinstance(last, ast.Return) or last.value is None or isinstance(last.value, (ast.Name, ast.Constant)):
        return None
    name = fresh(fn, "result_")
    fn.body[-1:] = [ast.Assign([ast.Name(name, ast.Store())], last.value), ast.Return(ast.Name(name))]
    return fn


def p_none(fn, rng):
    last = fn.body[-1]
    if isinstance(last, ast.Return) and (last.value is None or
                                         (isinstance(last.value, ast.Constant) and last.value.value is None)):
        if len(fn.body) == 1:
            return None
        fn.body.pop()
    elif not isinstance(last, (ast.Return, ast.Raise)):
        fn.body.append(ast.Return(ast.Constant(None)))
    else:
        return None
    return fn


def p_fold(fn, rng):
    sites = [n for n in int_constants(fn) if n.value >= 2]
    if not sites:
        return None
    node = rng.choice(sites)
    replacement = ast.BinOp(ast.Constant(node.value - 1), ast.Add(), ast.Constant(1))
    return replace_node(fn, node, replacement)


def p_pass(fn, rng):
    has_doc = isinstance(fn.body[0], ast.Expr) and isinstance(getattr(fn.body[0], "value", None), ast.Constant) \
        and isinstance(fn.body[0].value.value, str)
    fn.body.insert(1 if has_doc else 0, ast.Pass())
    return fn


PRESERVING = {"rename": p_rename, "ternary": p_ternary, "negate_swap": p_negate, "dead_code": p_dead,
              "temp_inline": p_temp, "implicit_none": p_none, "int_fold": p_fold, "pass": p_pass}


def p_combo(fn, rng):
    applied = 0
    for name in rng.sample(sorted(PRESERVING), len(PRESERVING)):
        out = PRESERVING[name](fn, rng)
        if out is not None:
            fn, applied = out, applied + 1
        if applied == 3:
            break
    return fn if applied >= 2 else None


# ------------------------------------------------------------------ breaking

def replace_node(fn, target, replacement):
    class Replace(ast.NodeTransformer):
        def generic_visit(self, node):
            return replacement if node is target else super().generic_visit(node)
    return Replace().visit(fn)


def local_names(fn):
    source = unparse(fn)
    table = symtable.symtable(source, "<fn>", "exec").get_children()[0]
    return {s.get_name() for s in table.get_symbols() if s.is_local() or s.is_parameter()}


def b_const(fn, rng):
    sites = int_constants(fn)
    if not sites:
        return None
    rng.choice(sites).value += 1
    return fn


def b_callee(fn, rng):
    locals_ = local_names(fn)
    sites = [n for n in ast.walk(fn) if isinstance(n, ast.Call)
             and ((isinstance(n.func, ast.Name) and n.func.id not in locals_) or isinstance(n.func, ast.Attribute))]
    if not sites:
        return None
    func = rng.choice(sites).func
    if isinstance(func, ast.Name):
        func.id += "_alt"
    else:
        func.attr += "_alt"
    return fn


def b_kwarg(fn, rng):
    sites = [k for k in ast.walk(fn) if isinstance(k, ast.keyword) and k.arg]
    if not sites:
        return None
    rng.choice(sites).arg += "_alt"
    return fn


def b_aug(fn, rng):
    sites = []
    for node in ast.walk(fn):
        if isinstance(node, ast.AugAssign) and isinstance(node.target, ast.Name):
            sites.append(("unfold", node))
        if (isinstance(node, ast.Assign) and len(node.targets) == 1 and isinstance(node.targets[0], ast.Name)
                and isinstance(node.value, ast.BinOp) and isinstance(node.value.left, ast.Name)
                and node.value.left.id == node.targets[0].id):
            sites.append(("fold", node))
    if not sites:
        return None
    kind, node = rng.choice(sites)
    if kind == "unfold":
        replacement = ast.Assign([ast.Name(node.target.id, ast.Store())],
                                 ast.BinOp(ast.Name(node.target.id), node.op, node.value))
    else:
        replacement = ast.AugAssign(ast.Name(node.targets[0].id, ast.Store()), node.value.op, node.value.right)
    return replace_node(fn, node, replacement)


def b_commute(fn, rng):
    sites = [n for n in ast.walk(fn) if isinstance(n, ast.BinOp) and isinstance(n.op, (ast.Add, ast.Mult))
             and ast.dump(n.left) != ast.dump(n.right)]
    if not sites:
        return None
    node = rng.choice(sites)
    node.left, node.right = node.right, node.left
    return fn


def b_demorgan(fn, rng):
    sites = [n for n in ast.walk(fn) if isinstance(n, (ast.If, ast.While)) and isinstance(n.test, ast.BoolOp)
             and isinstance(n.test.op, ast.And)]
    if not sites:
        return None
    node = rng.choice(sites)
    node.test = ast.UnaryOp(ast.Not(), ast.BoolOp(ast.Or(), [ast.UnaryOp(ast.Not(), v) for v in node.test.values]))
    return fn


FLIP = {ast.Lt: ast.Gt, ast.Gt: ast.Lt, ast.LtE: ast.GtE, ast.GtE: ast.LtE}


def b_flip(fn, rng):
    sites = [n for n in ast.walk(fn) if isinstance(n, ast.Compare) and len(n.ops) == 1 and type(n.ops[0]) in FLIP]
    if not sites:
        return None
    node = rng.choice(sites)
    node.left, node.comparators, node.ops = node.comparators[0], [node.left], [FLIP[type(node.ops[0])]()]
    return fn


def b_negate_only(fn, rng):
    sites = [n for n in ast.walk(fn) if isinstance(n, ast.If)]
    if not sites:
        return None
    node = rng.choice(sites)
    node.test = ast.UnaryOp(ast.Not(), node.test)
    return fn


CMP_CHANGE = {ast.Lt: ast.LtE, ast.LtE: ast.Lt, ast.Gt: ast.GtE, ast.GtE: ast.Gt, ast.Eq: ast.NotEq,
              ast.NotEq: ast.Eq, ast.Is: ast.IsNot, ast.IsNot: ast.Is, ast.In: ast.NotIn, ast.NotIn: ast.In}


def b_cmp(fn, rng):
    sites = [n for n in ast.walk(fn) if isinstance(n, ast.Compare) and type(n.ops[0]) in CMP_CHANGE]
    if not sites:
        return None
    node = rng.choice(sites)
    node.ops[0] = CMP_CHANGE[type(node.ops[0])]()
    return fn


def b_swap(fn, rng):
    sites = [(block, i) for block in blocks(fn) for i in range(len(block) - 1)
             if all(isinstance(s, ast.Expr) and isinstance(s.value, ast.Call) for s in block[i:i + 2])
             and ast.dump(block[i]) != ast.dump(block[i + 1])]
    if not sites:
        return None
    block, i = rng.choice(sites)
    block[i], block[i + 1] = block[i + 1], block[i]
    return fn


def b_default(fn, rng):
    sites = [d for d in fn.args.defaults + [d for d in fn.args.kw_defaults if d is not None]
             if isinstance(d, ast.Constant) and type(d.value) in (int, str)]
    if not sites:
        return None
    node = rng.choice(sites)
    node.value = node.value + 1 if type(node.value) is int else node.value + "_alt"
    return fn


def b_async(fn, rng):
    """A coroutine function returns an awaitable, not its value."""
    return ast.AsyncFunctionDef(fn.name, fn.args, fn.body, fn.decorator_list, fn.returns, fn.type_comment,
                                getattr(fn, "type_params", []))


def b_annotation(fn, rng):
    """FastAPI/pydantic-style frameworks validate by annotation, so it is behaviour."""
    sites = [a for a in fn.args.args + fn.args.kwonlyargs if a.annotation is not None]
    if not sites:
        return None
    rng.choice(sites).annotation = ast.Name("bytes")
    return fn


BREAKING = {"const": ("real", b_const), "callee": ("real", b_callee), "kwarg": ("real", b_kwarg),
            "async": ("real", b_async), "annotation": ("real", b_annotation),
            "negate_only": ("real", b_negate_only), "cmp": ("real", b_cmp), "swap_calls": ("real", b_swap),
            "default": ("real", b_default), "aug_assign": ("trap", b_aug), "commute": ("trap", b_commute),
            "demorgan": ("trap", b_demorgan), "cmp_flip": ("trap", b_flip)}


# ------------------------------------------------------------------ arms

def run_slop(pairs_path: Path) -> list[dict]:
    out = subprocess.run([str(SLOP), "equiv", str(pairs_path)], capture_output=True, text=True, check=True)
    return [json.loads(line) for line in out.stdout.splitlines()]


ALPHA_V1_EXAMPLE = r'''
use std::io::BufRead;
fn main() {
    for line in std::io::stdin().lock().lines() {
        let pair: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
        let alpha = |key: &str| slop_parse::Language::Python.parse(pair[key].as_str().unwrap())
            .ok().and_then(|f| f.into_iter().next()).map(|f| f.alpha_hash).unwrap_or_default();
        let (a, b) = (alpha("a"), alpha("b"));
        let verdict = if a.is_empty() || b.is_empty() { serde_json::Value::Null } else { (a == b).into() };
        println!("{}", serde_json::json!({ "alpha_v1": verdict }));
    }
}
'''


def run_alpha_v1(pairs_path: Path) -> list[dict]:
    """Rebuild the pinned commit's parser in a throwaway worktree; nothing lands in the repo."""
    with tempfile.TemporaryDirectory() as tmp:
        worktree = Path(tmp) / "alpha_v1"
        subprocess.run(["git", "-C", str(ROOT), "worktree", "add", "-q", "--detach", str(worktree),
                        ALPHA_V1_COMMIT], check=True)
        try:
            crate = worktree / "crates/slop-parse"
            (crate / "examples").mkdir(exist_ok=True)
            (crate / "examples/alpha_pairs.rs").write_text(ALPHA_V1_EXAMPLE)
            with (crate / "Cargo.toml").open("a") as manifest:
                manifest.write('\n[dev-dependencies]\nserde_json = "1"\n')
            subprocess.run(["cargo", "build", "-q", "--release", "--example", "alpha_pairs", "-p", "slop-parse"],
                           cwd=worktree, check=True, env={**os.environ,
                                                          "CARGO_TARGET_DIR": str(ROOT / "target/alpha_v1")})
            binary = ROOT / "target/alpha_v1/release/examples/alpha_pairs"
            out = subprocess.run([str(binary)], stdin=pairs_path.open(), capture_output=True, text=True, check=True)
            return [json.loads(line) for line in out.stdout.splitlines()]
        finally:
            subprocess.run(["git", "-C", str(ROOT), "worktree", "remove", "--force", str(worktree)], check=True)


def run_jscpd(pairs: list[dict]) -> list[bool | None]:
    """Token clone detection over each (a, b) file pair, in one jscpd run."""
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        for i, pair in enumerate(pairs):
            (root / f"p{i}_a.py").write_text(pair["a"])
            (root / f"p{i}_b.py").write_text(pair["b"])
        report = root / "report"
        subprocess.run(["npx", "--yes", "jscpd@4", "--silent", "--min-tokens", "20", "--reporters", "json",
                        "--output", str(report), str(root)], capture_output=True, check=False)
        found = set()
        report_file = report / "jscpd-report.json"
        if not report_file.exists():
            return [None] * len(pairs)
        for clone in json.loads(report_file.read_text()).get("duplicates", []):
            names = sorted(Path(clone[k]["name"]).name for k in ("firstFile", "secondFile"))
            if names[0].split("_")[0] == names[1].split("_")[0]:
                found.add(int(names[0].split("_")[0][1:]))
        return [i in found for i in range(len(pairs))]


# ------------------------------------------------------------------ statistics

def wilson(k: int, n: int, z: float = 1.96) -> tuple[float, float]:
    if n == 0:
        return (math.nan, math.nan)
    p = k / n
    centre = (p + z * z / (2 * n)) / (1 + z * z / n)
    half = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / (1 + z * z / n)
    return (max(0.0, centre - half), min(1.0, centre + half))


def rate(rows: list[dict], arm: str) -> dict:
    decided = [r[arm] for r in rows if r.get(arm) is not None]
    k, n = sum(bool(v) for v in decided), len(decided)
    low, high = wilson(k, n)
    return {"equal": k, "decided": n, "abstain": len(rows) - n, "rate": k / n if n else math.nan,
            "ci95": [low, high], "rule_of_three_upper": (3 / n if n and k == 0 else None)}


def percentile(values: list[float], q: float) -> float:
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, int(q * len(ordered)))] if ordered else math.nan


# ------------------------------------------------------------------ main

def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repos", nargs="*", type=Path, default=DEFAULT_REPOS)
    parser.add_argument("--limit", type=int, default=400)
    parser.add_argument("--seed", type=int, default=20260923)
    parser.add_argument("--jscpd", action="store_true")
    parser.add_argument("--out", type=Path, default=ROOT / "bench/results/equiv.jsonl")
    args = parser.parse_args()

    corpus = {}
    for key, fn in functions(args.repos):
        digest = hashlib.sha256(unparse(fn).encode()).hexdigest()
        corpus.setdefault(digest, (key, fn))
    rng = random.Random(args.seed)
    sample = rng.sample(sorted(corpus.values(), key=lambda kv: kv[0]), min(args.limit, len(corpus)))

    mutators = {**{name: ("sound", f) for name, f in PRESERVING.items()}, "combo": ("sound", p_combo), **BREAKING}
    pairs = []
    for key, fn in sample:
        original = unparse(fn)
        for name, (klass, mutate) in mutators.items():
            local = random.Random(f"{args.seed}:{key}:{name}")
            try:
                mutant = mutate(copy.deepcopy(fn), local)
                text = unparse(mutant) if mutant is not None else None
                if text is None or text == original:
                    continue
                ast.parse(text)
            except (SyntaxError, ValueError, AttributeError, TypeError, IndexError):
                continue
            pairs.append({"fn": key, "mutator": name, "class": klass,
                          "kind": "P" if klass == "sound" else "B", "a": original, "b": text})

    with tempfile.NamedTemporaryFile("w", suffix=".jsonl", delete=False) as handle:
        for pair in pairs:
            handle.write(json.dumps({"a": pair["a"], "b": pair["b"]}) + "\n")
        pairs_path = Path(handle.name)

    started = time.time()
    verdicts = run_slop(pairs_path)
    errors = [v for v in verdicts if "error" in v]
    alpha_v1 = run_alpha_v1(pairs_path)
    jscpd = run_jscpd(pairs) if args.jscpd else None
    rows = []
    for i, (pair, verdict) in enumerate(zip(pairs, verdicts)):
        if "error" in verdict:
            continue
        row = {**{k: pair[k] for k in ("fn", "mutator", "class", "kind")}, **verdict, **alpha_v1[i]}
        if jscpd is not None:
            row["jscpd"] = jscpd[i]
        rows.append(row)

    arms = ["exact", "structural", "alpha_v1", "alpha", "sound", "egg_sound", "graded", "egg_graded"]
    arms += ["jscpd"] if jscpd is not None else []
    groups = {"P (preserving: recall)": [r for r in rows if r["kind"] == "P"],
              "B real (false equivalence)": [r for r in rows if r["class"] == "real"],
              "B trap (false equivalence)": [r for r in rows if r["class"] == "trap"]}
    summary = {group: {arm: rate(members, arm) for arm in arms} for group, members in groups.items()}
    per_mutator = {name: {arm: rate([r for r in rows if r["mutator"] == name], arm) for arm in arms}
                   for name in mutators}
    latency = {key: {"p50": percentile([r[key] for r in rows if key in r], 0.5),
                     "p95": percentile([r[key] for r in rows if key in r], 0.95)}
               for key in ("normalize_us", "egg_sound_us", "egg_graded_us")}

    print(f"functions sampled: {len(sample)} of {len(corpus)} unique; pairs: {len(pairs)}; "
          f"slop errors: {len(errors)}; elapsed {time.time() - started:.1f}s\n")
    print("| group | " + " | ".join(arms) + " |")
    print("| --- | " + " | ".join("---:" for _ in arms) + " |")
    for group, by_arm in summary.items():
        cells = []
        for arm in arms:
            s = by_arm[arm]
            cells.append("n/a" if not s["decided"] else f"{s['rate']:.1%} ({s['equal']}/{s['decided']})")
        print(f"| {group} | " + " | ".join(cells) + " |")
    print("\nper mutator (share judged equal):")
    for name, by_arm in per_mutator.items():
        klass = mutators[name][0]
        cells = " ".join(f"{arm}={by_arm[arm]['rate']:.0%}" if by_arm[arm]["decided"] else f"{arm}=-" for arm in arms)
        print(f"  [{klass:5}] {name:14} n={sum(1 for r in rows if r['mutator'] == name):4}  {cells}")
    print(f"\nlatency (us): {json.dumps(latency)}")

    args.out.parent.mkdir(parents=True, exist_ok=True)
    manifest = {
        "bench": "equiv", "label": "exploratory", "at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "commit": subprocess.run(["git", "-C", str(ROOT), "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip(),
        "dirty": bool(subprocess.run(["git", "-C", str(ROOT), "status", "--porcelain", "--untracked-files=no", "--", "crates", "bench",
                                      ":!bench/__pycache__", ":!bench/results"],
                                     capture_output=True, text=True).stdout.strip()),
        "alpha_v1_commit": ALPHA_V1_COMMIT, "python": platform.python_version(), "platform": platform.platform(),
        "seed": args.seed, "repos": [str(r) for r in args.repos], "functions": len(sample), "pairs": len(pairs),
        "corpus_digest": hashlib.sha256("".join(sorted(corpus)).encode()).hexdigest(),
    }
    with args.out.open("a") as ledger:
        ledger.write(json.dumps({"manifest": manifest, "summary": summary, "per_mutator": per_mutator,
                                 "latency": latency}) + "\n")
    print(f"\nappended to {args.out}")


if __name__ == "__main__":
    sys.exit(main())
