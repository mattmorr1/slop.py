#!/usr/bin/env python3
"""Signal (1) spike: which functions are two functions concatenated?

Builds an intra-procedural data-dependence graph over the *top-level statements*
of each function body, then runs connected components. A function whose
statements fall into two or more components with no dataflow between them is
literally two functions spliced together, and the boundary is derivable rather
than a judgement call.

Deliberately a throwaway. Python's own `ast` gives exact Store/Load contexts, so
def-use here is better than the "name-approximated" version the plan budgeted
for, at a fraction of the cost of building a PDG in Rust. If the signal is real,
port it; if it isn't, this cost two hours.

Parameters are *not* treated as definitions. Two halves that both read the same
parameter are not thereby connected — that shared read is precisely the interface
width of the split, and counting it as a connection would collapse almost every
function to one component.

Known under-approximation of defs, which inflates component counts: mutation
through a call (`helper(out)` where helper mutates `out`) reads as a use, never a
def. Reported as `MUTATES?` so it can be eyeballed rather than silently trusted.

Usage:
    python3 bench/split_spike.py <repo> [--min-stmts 4] [--limit 40] [--json]
"""

from __future__ import annotations

import argparse
import ast
import json
import pathlib
import sys
from dataclasses import dataclass, field

SKIP_DIRS = {".git", "node_modules", ".venv", "venv", "target", "__pycache__", "build", "dist"}


def is_test(path: pathlib.Path) -> bool:
    parts = set(path.parts)
    return bool(parts & {"tests", "test"}) or path.name.startswith("test_") or path.name == "conftest.py"


def names(node: ast.AST, ctx: type) -> set[str]:
    """Identifiers under `node` in the given expression context."""
    return {n.id for n in ast.walk(node) if isinstance(n, ast.Name) and isinstance(n.ctx, ctx)}


def stmt_defs(stmt: ast.stmt) -> set[str]:
    """Names this statement binds. Store-context names cover assignment, for
    targets, with-as and comprehension targets; the rest are named explicitly."""
    out = names(stmt, ast.Store)
    for node in ast.walk(stmt):
        if isinstance(node, ast.ExceptHandler) and node.name:
            out.add(node.name)
        elif isinstance(node, ast.NamedExpr) and isinstance(node.target, ast.Name):
            out.add(node.target.id)
        elif isinstance(node, (ast.Import, ast.ImportFrom)):
            out |= {(a.asname or a.name).split(".")[0] for a in node.names}
        elif isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
            out.add(node.name)
    return out


def stmt_uses(stmt: ast.stmt) -> set[str]:
    return names(stmt, ast.Load)


def mutates_via_call(stmt: ast.stmt) -> bool:
    """`out.append(...)` — a method call on a bare name, where a definition may
    be happening that Store/Load contexts cannot see.

    Narrow on purpose: `any(Name in call.args)` would be true of nearly every
    call and the flag would carry no information.
    """
    return any(
        isinstance(n, ast.Call)
        and isinstance(n.func, ast.Attribute)
        and isinstance(n.func.value, ast.Name)
        for n in ast.walk(stmt)
    )


def exits(stmt: ast.stmt) -> bool:
    """Does this statement transfer control out of the enclosing block?

    A guard (`if not x: raise`) consumes its own value and then leaves, so it is
    *always* its own dataflow component. That makes disconnectedness worthless as
    evidence of "two functions" in any language where early return is idiomatic —
    the whole reason this flag exists.
    """
    if isinstance(stmt, (ast.Return, ast.Raise, ast.Break, ast.Continue)):
        return True
    return isinstance(stmt, ast.If) and bool(stmt.body) and exits(stmt.body[-1])


@dataclass
class Component:
    stmts: list[int] = field(default_factory=list)
    lines: tuple[int, int] = (0, 0)
    reads_params: set[str] = field(default_factory=set)
    free: set[str] = field(default_factory=set)
    guard: bool = False


class DisjointSet:
    def __init__(self, n: int) -> None:
        self.parent = list(range(n))

    def find(self, i: int) -> int:
        while self.parent[i] != i:
            self.parent[i] = self.parent[self.parent[i]]
            i = self.parent[i]
        return i

    def union(self, a: int, b: int) -> None:
        ra, rb = self.find(a), self.find(b)
        if ra != rb:
            self.parent[rb] = ra


def param_names(fn: ast.FunctionDef | ast.AsyncFunctionDef) -> set[str]:
    a = fn.args
    out = {p.arg for p in [*a.posonlyargs, *a.args, *a.kwonlyargs]}
    for extra in (a.vararg, a.kwarg):
        if extra:
            out.add(extra.arg)
    return out


def components_of(fn: ast.FunctionDef | ast.AsyncFunctionDef) -> list[Component]:
    body = [s for s in fn.body if not _is_docstring(s)]
    n = len(body)
    if n < 2:
        return []

    params = param_names(fn)
    defs = [stmt_defs(s) for s in body]
    uses = [stmt_uses(s) for s in body]

    ds = DisjointSet(n)
    for j in range(n):
        for i in range(j):
            # A later statement using a name an earlier one bound is dataflow.
            # Parameters need no exclusion here: nothing *defines* them, so two
            # statements that merely both read one share no def and stay
            # unlinked. Subtracting them would also delete the real edge created
            # when a statement rebinds a parameter (`rows = clean(rows)`).
            if defs[i] & uses[j]:
                ds.union(i, j)

    groups: dict[int, list[int]] = {}
    for i in range(n):
        groups.setdefault(ds.find(i), []).append(i)

    out = []
    for members in groups.values():
        stmts = sorted(members)
        bound = set().union(*(defs[i] for i in stmts)) if stmts else set()
        used = set().union(*(uses[i] for i in stmts)) if stmts else set()
        out.append(
            Component(
                stmts=stmts,
                lines=(body[stmts[0]].lineno, _end(body[stmts[-1]])),
                reads_params=used & params,
                free=used - bound - params,
                guard=any(exits(body[i]) for i in stmts),
            )
        )
    return sorted(out, key=lambda c: c.lines[0])


def _is_docstring(stmt: ast.stmt) -> bool:
    return isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Constant) and isinstance(stmt.value.value, str)


def _end(stmt: ast.stmt) -> int:
    return getattr(stmt, "end_lineno", stmt.lineno) or stmt.lineno


def scan(repo: pathlib.Path, min_stmts: int):
    findings = []
    files = 0
    functions = 0
    for path in sorted(repo.rglob("*.py")):
        if set(path.parts) & SKIP_DIRS or is_test(path.relative_to(repo)):
            continue
        try:
            tree = ast.parse(path.read_text(encoding="utf-8", errors="replace"))
        except SyntaxError:
            continue
        files += 1
        for fn in ast.walk(tree):
            if not isinstance(fn, (ast.FunctionDef, ast.AsyncFunctionDef)):
                continue
            body = [s for s in fn.body if not _is_docstring(s)]
            if len(body) < min_stmts:
                continue
            functions += 1
            comps = components_of(fn)
            real = [c for c in comps if len(c.stmts) >= 2]
            if len(real) < 2:
                continue
            findings.append(
                {
                    "file": str(path.relative_to(repo)),
                    "function": fn.name,
                    "line": fn.lineno,
                    "stmts": len(body),
                    "components": len(comps),
                    "substantial": len(real),
                    "mutates": any(mutates_via_call(s) for s in fn.body),
                    "parts": [
                        {
                            "lines": list(c.lines),
                            "stmts": len(c.stmts),
                            "reads_params": sorted(c.reads_params),
                            "guard": c.guard,
                        }
                        for c in comps
                        if len(c.stmts) >= 2
                    ],
                }
            )
    return findings, files, functions


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("repo", type=pathlib.Path)
    ap.add_argument("--min-stmts", type=int, default=4)
    ap.add_argument("--limit", type=int, default=40)
    ap.add_argument("--json", action="store_true")
    args = ap.parse_args()

    findings, files, functions = scan(args.repo, args.min_stmts)
    findings.sort(key=lambda f: (-f["substantial"], -f["stmts"]))

    if args.json:
        print(json.dumps(findings, indent=1))
        return 0

    print(f"{files} files, {functions} functions with >={args.min_stmts} statements")
    print(f"{len(findings)} with 2+ substantial dataflow components "
          f"({100 * len(findings) / max(functions, 1):.1f}%)\n")
    for f in findings[: args.limit]:
        flag = "  MUTATES?" if f["mutates"] else ""
        print(f"{f['file']}:{f['line']}  {f['function']}  "
              f"({f['stmts']} stmts -> {f['substantial']} parts){flag}")
        for p in f["parts"]:
            params = ", ".join(p["reads_params"]) or "-"
            print(f"    lines {p['lines'][0]:>5}-{p['lines'][1]:<5} {p['stmts']:>2} stmts   reads: {params}")
        print()
    return 0


if __name__ == "__main__":
    sys.exit(main())
