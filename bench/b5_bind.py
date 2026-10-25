#!/usr/bin/env python3
"""Binder v0 (B5 reference): rank the repository entities a sketch describes, over slop's outputs.

Four deterministic signals, merged by reciprocal rank (k = 60), ties by entity id (bench/B5_PREREGISTRATION.md):
name resolution, callee overlap and E-sound equality (both from MCP `assess_write`), and BM25 name
similarity for calls left unresolved. Stdlib only; the binary is only read, never changed.

  SLOP_BIN=target/release/slop python3 bench/b5_bind.py REPO FILE < sketch.py
"""
from __future__ import annotations

import ast
import builtins
import json
import subprocess
import sys
import textwrap
import time
from collections import defaultdict
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import reuse_bench as r8  # noqa: E402

RRF_K = 60
SUGGEST_LIMIT = 10
BM25_PER_CALL = 10
MAX_BODY_LINES = 30
BUILTINS = frozenset(dir(builtins))


def words(text: str) -> list[str]:
    return [w.lower() for w in r8.WORD.findall(text)]


def normalised(name: str) -> str:
    return name.lower().replace("_", "")


class Catalog:
    """Functions and classes outside tests that resolve to a def in a parsed .py file of `repo`."""

    def __init__(self, repo: Path):
        self.repo = repo
        self.entries, self.nodes, self.lines, self.docs, self.trees, self.files = {}, {}, {}, {}, {}, {}
        for entry in r8.slop_bench(repo, [], 1, [])[0]["catalog"]:
            file = self.files[entry["entity"]] = entry["file"]
            if not file.endswith(".py") or r8.is_test(file) or "tests" in entry["entity"].split("::"):
                continue
            tree = self.tree(file)
            kinds = (ast.ClassDef,) if entry["class"] else (ast.FunctionDef, ast.AsyncFunctionDef)
            lo, hi = entry["lines"]
            hits = tree and [n for n in ast.walk(tree) if isinstance(n, kinds) and n.name == r8.simple(entry["entity"])
                             and lo <= n.lineno - 1 <= hi]
            if hits:
                node = self.nodes[entry["entity"]] = min(hits, key=lambda n: n.lineno)
                self.entries[entry["entity"]] = {"file": file, "start": r8.start(node) - 1, "end": node.end_lineno - 1,
                                                 "name": node.name, "class": entry["class"]}
                self.docs[entry["entity"]] = words(node.name) + words(ast.get_docstring(node) or "")
        self.by_name, self.by_norm = defaultdict(list), defaultdict(list)
        for entity, entry in sorted(self.entries.items()):
            self.by_name[entry["name"]].append(entity)
            self.by_norm[normalised(entry["name"])].append(entity)
        self.bm25 = r8.Bm25(self.docs)
        self.cost_cache = {}

    def tree(self, file: str):
        if file not in self.trees:
            text = (self.repo / file).read_text(errors="replace")
            self.lines[file] = text.splitlines()
            try:
                self.trees[file] = ast.parse(text)
            except SyntaxError:
                self.trees[file] = None
        return self.trees[file]

    def cost(self, entity: str) -> int:
        """Tokens prewrite steering would show: a header, the body excerpt and one call site."""
        if entity not in self.cost_cache:
            entry = self.entries[entity]
            lines = self.lines[entry["file"]][entry["start"]: entry["end"] + 1]
            body = "\n".join(lines[:MAX_BODY_LINES]) + (f"\n    # ... {len(lines) - MAX_BODY_LINES} more lines" if len(lines) > MAX_BODY_LINES else "")
            site = self.call_site(entity)
            self.cost_cache[entity] = r8.tokens(f"# {entry['file']} ({entity})\n{body}\n" + (f"Used like this: `{site}`\n" if site else ""))
        return self.cost_cache[entity]

    def call_site(self, entity: str) -> str | None:
        # ponytail: first textual `name(` outside the definition, files in id order; graph callers if cost precision matters.
        entry, needle = self.entries[entity], f"{self.entries[entity]['name']}("
        for file in sorted(self.lines):
            for n, line in enumerate(self.lines[file]):
                if needle in line and not (file == entry["file"] and entry["start"] <= n <= entry["end"]) and "def " not in line:
                    return line.strip()
        return None


class Mcp:
    """One warm `slop mcp` process; newline-delimited JSON-RPC 2.0."""

    def __init__(self, repo: Path):
        self.repo, self.next_id = repo, 0
        self.proc = subprocess.Popen([str(r8.SLOP), "mcp", str(repo)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        self.request("initialize", {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "b5", "version": "0"}})

    def request(self, method: str, params: dict) -> dict:
        self.next_id += 1
        self.proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params}) + "\n")
        self.proc.stdin.flush()
        line = self.proc.stdout.readline()
        if not line:
            raise RuntimeError(f"slop mcp exited (code {self.proc.poll()})")
        reply = json.loads(line)
        if "error" in reply:
            raise RuntimeError(f"slop mcp {method}: {reply['error']}")
        return reply["result"]

    def tool(self, name: str, arguments: dict) -> dict:
        result = self.request("tools/call", {"name": name, "arguments": arguments})
        text = result["content"][0]["text"]
        if result.get("isError"):
            raise RuntimeError(f"slop mcp {name}: {text}")
        return json.loads(text)

    def close(self) -> None:
        self.proc.stdin.close()
        if self.proc.wait(timeout=30):
            raise RuntimeError(f"slop mcp exited with {self.proc.returncode}")


def sketch_calls(node, module_names: set[str]) -> list[str]:
    """Called names in source order: unbound non-builtin bare names, attributes not on an imported module."""
    bound, names = r8.bound(node), []
    for call in sorted((n for n in ast.walk(node) if isinstance(n, ast.Call)), key=lambda c: (c.lineno, c.col_offset)):
        func = call.func
        if isinstance(func, ast.Name) and func.id not in bound and func.id not in BUILTINS:
            names.append(func.id)
        elif isinstance(func, ast.Attribute):
            root = func.value
            while isinstance(root, ast.Attribute):
                root = root.value
            if not (isinstance(root, ast.Name) and root.id in module_names):
                names.append(func.attr)
    return list(dict.fromkeys(names))


def module_imports(tree) -> set[str]:
    return {(a.asname or a.name).split(".")[0] for s in (tree.body if tree else []) if isinstance(s, ast.Import) for a in s.names}


def imported_names(tree) -> set[str]:
    return {(a.asname or a.name).split(".")[0] for s in (tree.body if tree else []) if isinstance(s, (ast.Import, ast.ImportFrom)) for a in s.names}


class Binder:
    def __init__(self, repo: Path):
        self.catalog = Catalog(repo)
        self.mcp = Mcp(repo)

    def bind(self, node, file: str, target: str) -> dict:
        """Ranked entities for one parsed sketch function destined for `file`; `target` is never returned."""
        started, cat = time.perf_counter(), self.catalog
        source = ast.unparse(node)
        here = file.rsplit("/", 1)[0]
        near = lambda e: (cat.entries[e]["file"] != file, cat.entries[e]["file"].rsplit("/", 1)[0] != here, e)
        lists, resolved, unresolved = {}, [], []
        tree = cat.tree(file)
        imported = imported_names(tree)
        called = sketch_calls(node, module_imports(tree))
        for name in called:
            hits = cat.by_name.get(name) or cat.by_norm.get(normalised(name), [])
            if hits:
                resolved.extend(sorted(hits, key=near))
            elif name not in imported:
                unresolved.append(name)
        lists["name"] = resolved
        assessed = self.mcp.tool("assess_write", {"repo": str(cat.repo), "file": file, "content": source, "limit": SUGGEST_LIMIT})
        lists["overlap"] = [m["label"] for m in assessed["reuse_suggestions"]]
        lists["esound"] = [d["existing"]["label"] for d in assessed["duplicates"]]
        for name in unresolved:
            scores = cat.bm25.scores(words(name))
            lists[f"bm25:{name}"] = [e for _, e in sorted((-s, e) for e, s in scores.items())[:BM25_PER_CALL]]
        merged, via = defaultdict(float), defaultdict(set)
        for signal, ranked in lists.items():
            ranked = [e for e in dict.fromkeys(ranked) if e != target and e in cat.entries]
            for rank, entity in enumerate(ranked, 1):
                merged[entity] += 1 / (RRF_K + rank)
                via[entity].add(signal.split(":")[0])
        ranked = sorted(merged, key=lambda e: (-merged[e], e))
        return {"ranked": ranked, "via": {e: sorted(via[e]) for e in ranked[:20]},
                "esound": len(lists["esound"]), "calls": len(called), "unresolved": len(unresolved),
                "seconds": time.perf_counter() - started}


def main() -> None:
    repo, file = Path(sys.argv[1]).resolve(), sys.argv[2]
    tree = ast.parse(textwrap.dedent(sys.stdin.read()))
    node = next(n for n in ast.walk(tree) if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef)))
    binder = Binder(repo)
    result = binder.bind(node, file, "")
    binder.mcp.close()
    for entity in result["ranked"][:10]:
        print(f"{entity}  [{', '.join(result['via'].get(entity, []))}]  {binder.catalog.cost(entity)} tokens")
    print(f"{result['seconds'] * 1000:.1f} ms, {result['calls']} calls, {result['unresolved']} unresolved", file=sys.stderr)


if __name__ == "__main__":
    main()
