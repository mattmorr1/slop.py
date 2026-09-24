#!/usr/bin/env python3
"""R8: does the context a model sees change whether it reuses the repository's helpers?

Pre-registered in bench/R8_PREREGISTRATION.md. Three stages:

  prepare   select targets, gut them in a scratch copy, reindex, build four prompts per task
  generate  greedy generations from a local Ollama (stdlib only; runs on the GPU host)
  score     helper reuse and invented calls, paired bootstrap, appended to bench/results/reuse.jsonl

  SLOP_BIN=target/release/slop python3 bench/reuse_bench.py prepare \\
      --repo vigil=PATH:50 --repo flask=PATH:25 --repo httpx=PATH:25 --out DIR
  python3 bench/reuse_bench.py generate --tasks DIR/tasks.jsonl --model qwen2.5-coder:7b --out DIR/out.jsonl
  python3 bench/reuse_bench.py score --tasks DIR/tasks.jsonl --outputs DIR/out.jsonl
"""
from __future__ import annotations

import argparse
import ast
import builtins
import hashlib
import json
import math
import os
import platform
import random
import re
import shutil
import subprocess
import sys
import tempfile
import textwrap
import time
import urllib.request
from collections import Counter, defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SLOP = Path(os.environ.get("SLOP_BIN", ROOT / "target/release/slop"))
SEED = 20260923
ADAPTIVE_PPM = 10_000
# Hybrids show the target's file first, then other files' items in the remaining budget.
ARMS = ("none", "file", "bm25", "slop", "file+bm25", "file+slop")
WORD = re.compile(r"[A-Za-z][a-z0-9]*|[A-Z]+(?![a-z])|\d+")
FENCE = re.compile(r"```(?:python|py)?\s*\n(.*?)```", re.S)
IGNORE = shutil.ignore_patterns(".git", ".slop", "index.scip", "index.scip.*", "node_modules", ".venv", "venv",
                                "__pycache__", ".aider*", ".mypy_cache", ".pytest_cache")
INSTRUCTION = ("Implement the body of the function below. Reply with the complete function, signature "
               "included, in a single ```python code block and nothing else.")


def tokens(text: str) -> int:
    return len(text) // 4 + 1


def is_test(path: str) -> bool:
    name = path.rsplit("/", 1)[-1]
    return "/tests/" in f"/{path}" or "/test/" in f"/{path}" or name.startswith("test_") \
        or name.endswith("_test.py") or name == "conftest.py"


def simple(entity: str) -> str:
    return re.findall(r"[A-Za-z_][A-Za-z0-9_]*", entity.rsplit("::", 1)[-1])[-1]


def slop_bench(repo: Path, targets: list[str], budget: int, extra: list[str]) -> list[dict]:
    with tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False) as handle:
        handle.write("\n".join(targets) + "\n")
    out = subprocess.run([str(SLOP), "context-bench", str(repo), handle.name, "--budgets", str(budget), *extra],
                         capture_output=True, text=True, check=True)
    os.unlink(handle.name)
    return [json.loads(line) for line in out.stdout.splitlines()]


def function_node(tree: ast.Module, name: str, lo: int, hi: int):
    """The def named `name` whose line (1-based) lies in the entity's 0-based range."""
    hits = [node for node in ast.walk(tree) if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef))
            and node.name == name and lo <= node.lineno - 1 <= hi]
    return min(hits, key=lambda node: node.lineno) if hits else None


def start(stmt) -> int:
    """First line of a statement, counting its decorators (1-based)."""
    return min([d.lineno for d in getattr(stmt, "decorator_list", [])] + [stmt.lineno])


def body_after_docstring(node) -> list:
    body = node.body
    if body and isinstance(body[0], ast.Expr) and isinstance(getattr(body[0], "value", None), ast.Constant) \
            and isinstance(body[0].value.value, str):
        return body[1:]
    return body


def skeleton(lines: list[str], node) -> str:
    """Signature through the colon, docstring if any, then `...`: what bm25 packs."""
    first = start(node) - 1
    header_end = start(node.body[0]) - 1
    head = lines[first:header_end]
    doc = ast.get_docstring(node)
    indent = " " * (node.body[0].col_offset if node.body else node.col_offset + 4)
    parts = ["\n".join(head)]
    if doc:
        parts.append(f'{indent}"""{doc.strip()}"""')
    parts.append(f"{indent}...")
    return "\n".join(parts)


def pack(blocks, budget: int) -> list[str]:
    """Blocks in order, each kept if it still fits; headers count toward the budget."""
    kept, spent = [], 0
    for block in blocks:
        if spent + tokens(block) <= budget:
            kept.append(block)
            spent += tokens(block)
    return kept


def prepare(args) -> None:
    out, budget = args.out, args.budget
    out.mkdir(parents=True, exist_ok=True)
    tasks = []
    for spec in args.repo:
        name, rest = spec.split("=", 1)
        source, count = rest.rsplit(":", 1)
        source, count = Path(source).expanduser().resolve(), int(count)
        lines_out = slop_bench(source, [], 1, [])
        catalog = {entry["entity"]: entry for entry in lines_out[0]["catalog"]}
        internal = {entity for entity, entry in catalog.items() if not is_test(entry["file"])}
        trees, texts = {}, {}

        def parsed(path: str):
            if path not in trees:
                texts[path] = (source / path).read_text(errors="replace")
                try:
                    trees[path] = ast.parse(texts[path])
                except SyntaxError:
                    trees[path] = None
            return trees[path]

        candidates = []
        for entity, entry in sorted(catalog.items()):
            if entry["class"] or not entry["file"].endswith(".py") or is_test(entry["file"]) or "tests" in entity.split("::"):
                continue
            tree = parsed(entry["file"])
            node = tree and function_node(tree, simple(entity), *entry["lines"])
            body = node and body_after_docstring(node)
            if body and 5 <= body[-1].end_lineno - start(body[0]) + 1 <= 60:
                candidates.append(entity)
        rows = slop_bench(source, candidates, 1, [])
        callees = {row["target"]: sorted(set(row["callees"]) & internal - {row["target"]}) for row in rows if "callees" in row}
        eligible = [entity for entity in candidates if 1 <= len(callees.get(entity, [])) <= 10]
        random.Random(f"{SEED}:{name}").shuffle(eligible)
        picked, per_file, forbidden = [], Counter(), set()
        for entity in eligible:
            file = catalog[entity]["file"]
            mine = set(callees[entity])
            if per_file[file] >= 2 or entity in forbidden or mine & set(picked):
                continue
            picked.append(entity)
            per_file[file] += 1
            forbidden |= mine
            if len(picked) == count:
                break
        if len(picked) < count:
            raise SystemExit(f"{name}: only {len(picked)} eligible targets for {count}")

        copy = out / f"{name}-gutted"
        if copy.exists():
            shutil.rmtree(copy)
        shutil.copytree(source, copy, ignore=IGNORE, symlinks=True)
        by_file = defaultdict(list)
        for entity in picked:
            by_file[catalog[entity]["file"]].append(entity)
        for file, entities in by_file.items():
            lines = texts[file].splitlines()
            nodes = sorted((function_node(trees[file], simple(e), *catalog[e]["lines"]) for e in entities),
                           key=lambda node: -node.lineno)
            for node in nodes:
                body = body_after_docstring(node)
                indent = " " * body[0].col_offset
                lines[start(body[0]) - 1: body[-1].end_lineno] = [f"{indent}raise NotImplementedError"]
            (copy / file).write_text("\n".join(lines) + "\n")
        subprocess.run([str(SLOP), "index", str(copy)], check=True, stdout=subprocess.DEVNULL)

        gutted_rows = slop_bench(copy, picked, budget, ["--min-probability-ppm", str(ADAPTIVE_PPM), "--with-text"])
        envelopes = {row["target"]: row for row in gutted_rows if "items" in row}
        gutted_catalog = {e["entity"]: e for e in gutted_rows[0]["catalog"]}
        gutted_trees, gutted_text = {}, {}
        for entry in gutted_catalog.values():
            if entry["file"].endswith(".py") and entry["file"] not in gutted_text:
                gutted_text[entry["file"]] = (copy / entry["file"]).read_text(errors="replace")
                try:
                    gutted_trees[entry["file"]] = ast.parse(gutted_text[entry["file"]])
                except SyntaxError:
                    gutted_trees[entry["file"]] = None
        skeletons = {}
        for entity, entry in gutted_catalog.items():
            tree = gutted_trees.get(entry["file"])
            if tree is None:
                continue
            kinds = (ast.ClassDef,) if entry["class"] else (ast.FunctionDef, ast.AsyncFunctionDef)
            hits = [n for n in ast.walk(tree) if isinstance(n, kinds) and n.name == simple(entity)
                    and entry["lines"][0] <= n.lineno - 1 <= entry["lines"][1]]
            if hits:
                skeletons[entity] = skeleton(gutted_text[entry["file"]].splitlines(), min(hits, key=lambda n: n.lineno))
        index = Bm25({entity: WORD.findall(text.lower()) for entity, text in skeletons.items()})
        repo_names = {simple(entity) for entity in catalog}

        for entity in picked:
            file = catalog[entity]["file"]
            text = gutted_text[file]
            lines = text.splitlines()
            tree = gutted_trees[file]
            node = function_node(tree, simple(entity), *gutted_catalog[entity]["lines"])
            first = start(node) - 1
            stub = "\n".join(lines[first: node.end_lineno])
            imports = "\n".join(ast.get_source_segment(text, stmt) for stmt in tree.body
                                if isinstance(stmt, (ast.Import, ast.ImportFrom)))
            top_level = set()
            for stmt in tree.body:
                if isinstance(stmt, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
                    top_level.add(stmt.name)
                elif isinstance(stmt, (ast.Import, ast.ImportFrom)):
                    top_level |= {(alias.asname or alias.name).split(".")[0] for alias in stmt.names}
                elif isinstance(stmt, (ast.Assign, ast.AnnAssign)):
                    for target in (stmt.targets if isinstance(stmt, ast.Assign) else [stmt.target]):
                        top_level |= {n.id for n in ast.walk(target) if isinstance(n, ast.Name)}
            window = budget * 4
            if len(text) <= window:
                file_context = text
            else:
                centre = len("\n".join(lines[:first]))
                lo = max(0, min(centre - window // 2, len(text) - window))
                file_context = text[lo: lo + window]
            ranked = sorted(((score, e) for e, score in index.scores(WORD.findall(stub.lower())).items() if e != entity),
                            key=lambda pair: (-pair[0], pair[1]))
            bm25_blocks = [(gutted_catalog[o]["file"], f"# {gutted_catalog[o]['file']} ({o})\n{skeletons[o]}\n") for _, o in ranked]
            # Same packing rule as bm25: headers count, items kept in the envelope's own order.
            slop_blocks = [(gutted_catalog.get(i["entity"], {}).get("file", "?"),
                            f"# {gutted_catalog.get(i['entity'], {}).get('file', '?')} ({i['entity']})\n{i['text']}\n")
                           for i in envelopes[entity]["items"] if i["entity"] != entity]
            file_block = f"\nCurrent contents of {file} (the function is a stub):\n{file_context}\n"
            rest = max(0, budget - tokens(file_block))
            elsewhere = lambda blocks: "".join(pack((b for f, b in blocks if f != file), rest))
            header = f"Repository: {name}. File: {file}\nImports at the top of the file:\n{imports}\n"
            blocks = {
                "none": "",
                "file": file_block,
                "bm25": "\nRelated code from the repository:\n" + "".join(pack((b for _, b in bm25_blocks), budget)),
                "slop": "\nRelated code from the repository:\n" + "".join(pack((b for _, b in slop_blocks), budget)),
                "file+bm25": file_block + "\nRelated code from other files:\n" + elsewhere(bm25_blocks),
                "file+slop": file_block + "\nRelated code from other files:\n" + elsewhere(slop_blocks),
            }
            prompts = {arm: f"{header}{block}\n{INSTRUCTION}\n\n```python\n{stub}\n```\n" for arm, block in blocks.items()}
            tasks.append({
                "id": f"{name}:{entity}", "repo": name, "target": entity, "name": node.name, "file": file,
                "callees": callees[entity], "callee_names": sorted({simple(c) for c in callees[entity]}),
                "cross_file_names": sorted({simple(c) for c in callees[entity] if catalog[c]["file"] != file}),
                "budget": budget,
                "allowed": sorted(repo_names | top_level),
                "prompts": prompts, "prompt_tokens": {arm: tokens(p) for arm, p in prompts.items()},
            })
        print(f"{name}: {len(picked)} targets gutted and indexed in {copy}", file=sys.stderr)
    (out / "tasks.jsonl").write_text("".join(json.dumps(task) + "\n" for task in tasks))
    digest = hashlib.sha256((out / "tasks.jsonl").read_bytes()).hexdigest()
    print(f"{len(tasks)} tasks -> {out / 'tasks.jsonl'} (sha256 {digest})")


class Bm25:
    def __init__(self, docs: dict[str, list[str]], k1: float = 1.2, b: float = 0.75):
        self.docs, self.k1, self.b = docs, k1, b
        self.avg = sum(map(len, docs.values())) / max(1, len(docs))
        self.df = Counter(term for words in docs.values() for term in set(words))
        self.tf = {ident: Counter(words) for ident, words in docs.items()}
        self.postings = defaultdict(list)
        for ident, counts in self.tf.items():
            for term in counts:
                self.postings[term].append(ident)

    def scores(self, query: list[str]) -> dict[str, float]:
        n, scores = len(self.docs), defaultdict(float)
        for term in set(query):
            idf = math.log(1 + (n - self.df[term] + 0.5) / (self.df[term] + 0.5))
            for ident in self.postings.get(term, ()):
                tf, length = self.tf[ident][term], len(self.docs[ident])
                scores[ident] += idf * tf * (self.k1 + 1) / (tf + self.k1 * (1 - self.b + self.b * length / self.avg))
        return scores


def generate(args) -> None:
    done = set()
    if args.out.exists():
        done = {(r["id"], r["arm"], r["model"]) for r in map(json.loads, args.out.read_text().splitlines())}
    tasks = [json.loads(line) for line in args.tasks.read_text().splitlines()]
    arms = args.arms.split(",")
    todo = [(t, arm) for t in tasks for arm in arms if (t["id"], arm, args.model) not in done]
    with args.out.open("a") as out:
        for n, (task, arm) in enumerate(todo):
            # Reasoning traces off: they spend the output budget and are not the answer.
            body = json.dumps({"model": args.model, "prompt": task["prompts"][arm], "stream": False, "think": False,
                               "options": {"temperature": 0, "seed": 0, "num_ctx": args.num_ctx, "num_predict": 768}})
            request = urllib.request.Request(f"{args.host}/api/generate", data=body.encode(),
                                             headers={"Content-Type": "application/json"})
            started = time.time()
            with urllib.request.urlopen(request, timeout=600) as response:
                reply = json.loads(response.read())
            out.write(json.dumps({"id": task["id"], "arm": arm, "model": args.model, "response": reply["response"],
                                  "prompt_eval_count": reply.get("prompt_eval_count"),
                                  "eval_count": reply.get("eval_count"), "seconds": round(time.time() - started, 2)}) + "\n")
            out.flush()
            print(f"{n + 1}/{len(todo)} {args.model} {arm} {task['id']} {time.time() - started:.1f}s", file=sys.stderr, flush=True)


def generated_function(response: str, name: str):
    match = FENCE.search(response)
    # Methods come back at their class indentation, as the stub showed them.
    code = textwrap.dedent(match.group(1) if match else response)
    try:
        tree = ast.parse(code)
    except SyntaxError:
        return None
    defs = [n for n in ast.walk(tree) if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef))]
    named = [n for n in defs if n.name == name]
    return (named or defs or [None])[0]


def calls(node) -> tuple[set[str], set[str]]:
    bare, any_name = set(), set()
    for call in (n for n in ast.walk(node) if isinstance(n, ast.Call)):
        if isinstance(call.func, ast.Name):
            bare.add(call.func.id)
            any_name.add(call.func.id)
        elif isinstance(call.func, ast.Attribute):
            any_name.add(call.func.attr)
    return bare, any_name


def bound(node) -> set[str]:
    names = set()
    for n in ast.walk(node):
        if isinstance(n, ast.arg):
            names.add(n.arg)
        elif isinstance(n, ast.Name) and isinstance(n.ctx, (ast.Store, ast.Del)):
            names.add(n.id)
        elif isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
            names.add(n.name)
        elif isinstance(n, (ast.Import, ast.ImportFrom)):
            names |= {(a.asname or a.name).split(".")[0] for a in n.names}
        elif isinstance(n, ast.ExceptHandler) and n.name:
            names.add(n.name)
        elif isinstance(n, (ast.Global, ast.Nonlocal)):
            names |= set(n.names)
    return names


def bootstrap(values: list[float], rounds: int = 1000) -> tuple[float, float, float]:
    if not values:
        return (math.nan, math.nan, math.nan)
    rng = random.Random(SEED)
    means = sorted(sum(values[rng.randrange(len(values))] for _ in values) / len(values) for _ in range(rounds))
    return (sum(values) / len(values), means[int(0.025 * rounds)], means[int(0.975 * rounds)])


def score(args) -> None:
    tasks = {t["id"]: t for t in map(json.loads, args.tasks.read_text().splitlines())}
    outputs = [json.loads(line) for path in args.outputs for line in path.read_text().splitlines()]
    builtin_names = set(dir(builtins))
    rows = {}
    for out in outputs:
        task = tasks[out["id"]]
        node = generated_function(out["response"], task["name"])
        cross = task.get("cross_file_names", [])
        if node is None:
            rows[(out["model"], out["id"], out["arm"])] = {"reuse": 0.0, "cross": 0.0 if cross else None, "invented": None, "parse_error": True}
            continue
        bare, any_name = calls(node)
        allowed = set(task["allowed"]) | builtin_names | bound(node)
        rows[(out["model"], out["id"], out["arm"])] = {
            "reuse": sum(name in any_name for name in task["callee_names"]) / len(task["callee_names"]),
            "cross": sum(name in any_name for name in cross) / len(cross) if cross else None,
            "invented": bool(bare - allowed), "parse_error": False}
    arms = [arm for arm in ARMS if any(key[2] == arm for key in rows)]
    models = sorted({key[0] for key in rows})
    repos = sorted({t["repo"] for t in tasks.values()})
    complete = [(m, i) for m in models for i in tasks if all((m, i, arm) in rows for arm in arms)]
    missing = len(models) * len(tasks) - len(complete)
    compare = [pair.split(":") for pair in args.compare]
    invented_compare = [pair.split(":") for pair in args.invented]

    def select(model=None, repo=None):
        return [(m, i) for m, i in complete if (model is None or m == model) and (repo is None or tasks[i]["repo"] == repo)]

    def paired(keys, a, b, metric):
        values = [(rows[(m, i, a)][metric], rows[(m, i, b)][metric]) for m, i in keys]
        return bootstrap([float(x) - float(y) for x, y in values if x is not None and y is not None])

    results = {}
    scopes = [("pooled", None, None)] + [(f"repo={r}", None, r) for r in repos] + [(f"model={m}", m, None) for m in models]
    for label, model, repo in scopes:
        keys = select(model, repo)
        entry = {"tasks": len(keys)}
        for arm in arms:
            entry[arm] = {
                "reuse": bootstrap([rows[(m, i, arm)]["reuse"] for m, i in keys]),
                "cross_file_reuse": bootstrap([rows[(m, i, arm)]["cross"] for m, i in keys if rows[(m, i, arm)]["cross"] is not None]),
                "invented": bootstrap([float(rows[(m, i, arm)]["invented"]) for m, i in keys if rows[(m, i, arm)]["invented"] is not None]),
                "parse_errors": sum(rows[(m, i, arm)]["parse_error"] for m, i in keys),
                "prompt_tokens": sum(tasks[i]["prompt_tokens"][arm] for _, i in keys) / max(1, len(keys)),
            }
        for a, b in compare:
            entry[f"{a}_minus_{b}_reuse"] = paired(keys, a, b, "reuse")
            entry[f"{a}_minus_{b}_cross_file_reuse"] = paired(keys, a, b, "cross")
        for a, b in invented_compare:
            entry[f"{a}_minus_{b}_invented"] = paired(keys, a, b, "invented")
        results[label] = entry

    pct = lambda e: f"{e[0] * 100:5.1f}% [{e[1] * 100:.1f}, {e[2] * 100:.1f}]"
    diff = lambda e: f"{e[0] * 100:+5.1f} [{e[1] * 100:+.1f}, {e[2] * 100:+.1f}]"
    for label, entry in results.items():
        print(f"\n{label} ({entry['tasks']} task x model pairs)")
        print("| arm | reuse | cross-file reuse | invented calls | parse errors | prompt tokens |")
        print("| --- | ---: | ---: | ---: | ---: | ---: |")
        for arm in arms:
            x = entry[arm]
            print(f"| {arm} | {pct(x['reuse'])} | {pct(x['cross_file_reuse'])} | {pct(x['invented'])} | {x['parse_errors']} | {x['prompt_tokens']:.0f} |")
        for a, b in compare:
            print(f"{a} - {b}: reuse {diff(entry[f'{a}_minus_{b}_reuse'])}, cross-file {diff(entry[f'{a}_minus_{b}_cross_file_reuse'])}")
        for a, b in invented_compare:
            print(f"{a} - {b}: invented {diff(entry[f'{a}_minus_{b}_invented'])} (margin +5)")
    if missing:
        print(f"\n{missing} (task, model) pairs lack an arm and were left out", file=sys.stderr)
    manifest = {"bench": "reuse", "label": f"pre-registered (bench/R8_PREREGISTRATION.md), {args.run}",
                "at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
                "commit": subprocess.run(["git", "-C", str(ROOT), "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip(),
                "tasks_digest": hashlib.sha256(args.tasks.read_bytes()).hexdigest(),
                "arms": arms, "models": models, "python": platform.python_version(), "missing_pairs": missing}
    ledger = ROOT / "bench/results/reuse.jsonl"
    with ledger.open("a") as handle:
        handle.write(json.dumps({"manifest": manifest, "results": results}) + "\n")
    print(f"\nappended to {ledger}")


def main() -> None:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="stage", required=True)
    p = sub.add_parser("prepare")
    p.add_argument("--repo", action="append", required=True, help="NAME=PATH:COUNT")
    p.add_argument("--out", type=Path, required=True)
    p.add_argument("--budget", type=int, default=4000)
    g = sub.add_parser("generate")
    g.add_argument("--tasks", type=Path, required=True)
    g.add_argument("--model", required=True)
    g.add_argument("--out", type=Path, required=True)
    g.add_argument("--host", default="http://127.0.0.1:11434")
    g.add_argument("--arms", default="none,file,bm25,slop")
    g.add_argument("--num-ctx", type=int, default=8192)
    s = sub.add_parser("score")
    s.add_argument("--tasks", type=Path, required=True)
    s.add_argument("--outputs", type=Path, nargs="+", required=True)
    s.add_argument("--run", default="run 1")
    s.add_argument("--compare", nargs="*", default=["slop:bm25", "slop:file", "slop:none"], help="A:B reuse differences")
    s.add_argument("--invented", nargs="*", default=["slop:file"], help="A:B invented-call differences")
    args = parser.parse_args()
    {"prepare": prepare, "generate": generate, "score": score}[args.stage](args)


if __name__ == "__main__":
    main()
