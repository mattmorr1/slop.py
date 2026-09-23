#!/usr/bin/env python3
"""B4: context selection, measured without a model.

Ground truth is history. In each past commit that changed 2-20 non-test
functions that still exist, every changed function is in turn the edit target
and the rest of that commit's functions are what the edit needed in view
(leave-one-out co-change). A selector sees the target and a token budget;
recall@B is the share of that gold set it included. Tasks from one commit are
correlated, so confidence intervals come from a bootstrap over commits.

Every arm is charged alike: slop by its own renderings, every other arm by
slop's skeleton cost for each entity (the catalog), so no arm gets cheaper text.

Arms: slop coverage and slop ranked (the envelope's selections), BM25 over
function source, same-file/nearby-line proximity, a seeded random floor, and
`calibrated`: logistic regression over (graph distance, same file, line gap,
same directory, same container, shared effect, class-ness, BM25 similarity),
fit leave-one-repo-out, packed by probability (rank) or probability per token
(ratio: the greedy for expected recall under a budget).

Leakage, declared: selectors run on today's graph, which may hold call edges a
gold commit itself added. `no-edge` scores only gold with no direct edge to
the target, where that shortcut cannot help.

Usage:
  cargo build --release -p slop-cli
  python3 bench/context_bench.py --repo NAME=HISTORY:SNAPSHOT [--repo ...]
"""
from __future__ import annotations

import argparse
import ast
import hashlib
import json
import math
import os
import platform
import random
import re
import subprocess
import tempfile
import time
from collections import Counter, defaultdict
from pathlib import Path

import numpy as np

ROOT = Path(__file__).resolve().parent.parent
SLOP = Path(os.environ.get("SLOP_BIN", ROOT / "target/release/slop"))
# Adaptive arms stop at this calibrated probability instead of filling the budget.
THRESHOLDS = (0.2, 0.05, 0.01)
HUNK = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@")
WORD = re.compile(r"[A-Za-z][a-z0-9]*|[A-Z]+(?![a-z])|\d+")
FEATURES = ["bias", "d1", "d2", "d3", "d4", "same_file", "same_dir", "file_gap", "same_container",
            "shared_effect", "is_class", "lexical"]
NEGATIVES_PER_TASK = 200


def is_test(path: str) -> bool:
    name = path.rsplit("/", 1)[-1]
    return "/tests/" in f"/{path}" or "/test/" in f"/{path}" or name.startswith("test_") \
        or name.endswith("_test.py") or name == "conftest.py"


def entity_id(path: str, qualname: str, prefix: str) -> str:
    module = path[:-3]
    module = module[len(prefix):] if prefix and module.startswith(prefix) else module
    module = module.replace("/", ".")
    module = module[: -len(".__init__")] if module.endswith(".__init__") else module
    return f"{module}::{qualname.replace('.', '::')}"


def functions(source: str):
    out = []

    def visit(node, prefix):
        for child in ast.iter_child_nodes(node):
            if isinstance(child, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
                qual = f"{prefix}{child.name}"
                if not isinstance(child, ast.ClassDef):
                    out.append((qual, child.lineno, child.end_lineno))
                visit(child, qual + ".")

    visit(ast.parse(source), "")
    return out


def git(repo: Path, *args: str) -> str:
    return subprocess.run(["git", "-C", str(repo), *args], capture_output=True, text=True, check=True).stdout


def module_prefix(catalog: dict) -> str:
    """Source-layout prefix (`src/`) that SCIP module ids omit, inferred from the catalog."""
    for entry in catalog.values():
        path, module = entry["file"][:-3], entry["entity"].split("::")[0].replace(".", "/")
        if path.endswith(module) and path != module:
            return path[: len(path) - len(module)]
    return ""


def mine(repo: str, history: Path, catalog: dict, max_commits: int):
    prefix = module_prefix(catalog)
    tasks = []
    log = git(history, "log", "--no-merges", f"-n{max_commits}", "--format=%H %ct", "--", "*.py").split("\n")
    for line in filter(None, log):
        sha, stamp = line.split()
        diff = git(history, "show", "--format=", "--unified=0", "--no-color", sha, "--", "*.py")
        ranges, current = defaultdict(list), None
        for row in diff.splitlines():
            if row.startswith("+++ "):
                current = row[6:] if row.startswith("+++ b/") else None
            elif current and (match := HUNK.match(row)):
                start, count = int(match[1]), int(match[2] if match[2] is not None else 1)
                ranges[current].append((start, start + max(count, 1) - 1))
        changed = set()
        for path, spans in ranges.items():
            if is_test(path):
                continue
            try:
                defs = functions(git(history, "show", f"{sha}:{path}"))
            except (subprocess.CalledProcessError, SyntaxError, ValueError):
                continue
            for qual, first, last in defs:
                ident = entity_id(path, qual, prefix)
                if ident in catalog and any(min(last, hi) >= max(first, lo) for lo, hi in spans):
                    changed.add(ident)
        if 2 <= len(changed) <= 20:
            for target in sorted(changed):
                tasks.append({"repo": repo, "commit": sha, "time": int(stamp), "target": target,
                              "gold": sorted(changed - {target})})
    return tasks


def slop_runs(snapshot: Path, targets: list[str], budgets: list[int], selection: str):
    with tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False) as handle:
        handle.write("\n".join(targets) + "\n")
    out = subprocess.run([str(SLOP), "context-bench", str(snapshot), handle.name, "--budgets",
                          ",".join(map(str, budgets)), "--selection", selection],
                         capture_output=True, text=True, check=True)
    lines = [json.loads(line) for line in out.stdout.splitlines()]
    header, graph, picks, micros = lines[0], {}, {}, []
    for line in lines[1:]:
        if "neighbors" in line:
            graph[line["target"]] = (set(line["neighbors"]), dict((e, d) for e, d in line["distances"]))
        else:
            picks[(line["target"], line["budget"])] = {i["entity"] for i in line["items"]} - {line["target"]}
            micros.append(line["micros"])
    return header, graph, picks, micros


def pack(order, cost: dict[str, int], budget: int) -> set[str]:
    chosen, spent = set(), 0
    for ident in order:
        if spent + cost[ident] <= budget:
            chosen.add(ident)
            spent += cost[ident]
    return chosen


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


def words(text: str) -> list[str]:
    return [w.lower() for w in WORD.findall(text) if len(w) > 1]


def container(ident: str) -> str:
    return ident.rsplit("::", 1)[0]


class Repo:
    """One repository's snapshot view: catalog, sources, lexical index, slop runs."""

    def __init__(self, name: str, history: Path, snapshot: Path, budgets: list[int], max_commits: int):
        self.name, self.history, self.snapshot = name, history, snapshot
        self.header, _, _, _ = slop_runs(snapshot, [], budgets, "coverage")
        self.catalog = {entry["entity"]: entry for entry in self.header["catalog"]}
        self.tasks = mine(name, history, self.catalog, max_commits)
        self.sources = {}
        for ident, entry in self.catalog.items():
            path = snapshot / entry["file"]
            if path.suffix == ".py" and path.exists():
                lines = path.read_text(errors="replace").splitlines()
                self.sources[ident] = words("\n".join(lines[entry["lines"][0]: entry["lines"][1] + 1]))
        self.bm25 = Bm25(self.sources)
        self.cost = {ident: entry["skeleton_tokens"] for ident, entry in self.catalog.items()}
        self.by_dir = defaultdict(list)
        for ident, entry in self.catalog.items():
            self.by_dir[entry["file"].rsplit("/", 1)[0] if "/" in entry["file"] else ""].append(ident)

    def run_slop(self, budgets: list[int]):
        targets = sorted({task["target"] for task in self.tasks})
        self.picks, self.micros = {}, {}
        for selection in ("coverage", "ranked"):
            _, self.graph, self.picks[selection], self.micros[selection] = slop_runs(
                self.snapshot, targets, budgets, selection)

    def candidates(self, target: str):
        """Graph neighbourhood (<= 4 hops) plus the target's directory, with features."""
        entry = self.catalog[target]
        _, distances = self.graph.get(target, (set(), {}))
        directory = entry["file"].rsplit("/", 1)[0] if "/" in entry["file"] else ""
        pool = (set(distances) & set(self.catalog)) | set(self.by_dir[directory])
        pool.discard(target)
        lexical = self.bm25.scores(self.sources.get(target, []))
        target_effects = set(entry["effects"])
        rows, idents = [], sorted(pool)
        for ident in idents:
            other = self.catalog[ident]
            d = distances.get(ident, 0)
            same_file = other["file"] == entry["file"]
            gap = abs(other["lines"][0] - entry["lines"][0])
            other_dir = other["file"].rsplit("/", 1)[0] if "/" in other["file"] else ""
            rows.append([1.0, d == 1, d == 2, d == 3, d == 4, same_file, other_dir == directory and not same_file,
                         math.log1p(gap) if same_file else 0.0, container(ident) == container(target),
                         bool(target_effects & set(other["effects"])), other["class"],
                         math.log1p(lexical.get(ident, 0.0))])
        return idents, np.array(rows, dtype=float)


def fit(rows: np.ndarray, labels: np.ndarray, weights: np.ndarray, l2: float = 1.0, steps: int = 30) -> np.ndarray:
    """L2-regularised logistic regression by Newton's method (IRLS)."""
    beta = np.zeros(rows.shape[1])
    penalty = l2 * np.eye(rows.shape[1])
    penalty[0, 0] = 0.0
    for _ in range(steps):
        p = 1 / (1 + np.exp(-rows @ beta))
        gradient = rows.T @ (weights * (p - labels)) + penalty @ beta
        hessian = (rows * (weights * p * (1 - p))[:, None]).T @ rows + penalty
        step = np.linalg.solve(hessian, gradient)
        beta -= step
        if np.abs(step).max() < 1e-8:
            break
    return beta


def training_rows(repos: list[Repo], seed: int):
    rng = random.Random(seed)
    rows, labels, weights = [], [], []
    for repo in repos:
        for task in repo.tasks:
            idents, features = repo.candidates(task["target"])
            gold = set(task["gold"])
            positives = [i for i, ident in enumerate(idents) if ident in gold]
            negatives = [i for i, ident in enumerate(idents) if ident not in gold]
            kept = rng.sample(negatives, min(NEGATIVES_PER_TASK, len(negatives)))
            # Negative subsampling: weight kept negatives back up so probabilities stay calibrated.
            scale = len(negatives) / max(1, len(kept))
            for i, label, weight in [(i, 1.0, 1.0) for i in positives] + [(i, 0.0, scale) for i in kept]:
                rows.append(features[i])
                labels.append(label)
                weights.append(weight)
    return np.array(rows), np.array(labels), np.array(weights)


def model_json(beta: np.ndarray, source: str) -> dict:
    return {"features": FEATURES, "weights": [round(float(w), 6) for w in beta], "source": source}


def cluster_bootstrap(values: dict[str, list[float]], seed: int, rounds: int = 1000):
    clusters = [v for v in values.values() if v]
    if not clusters:
        return (math.nan, math.nan, math.nan)
    flat = [x for cluster in clusters for x in cluster]
    rng, means = random.Random(seed), []
    for _ in range(rounds):
        sample = [x for _ in clusters for x in clusters[rng.randrange(len(clusters))]]
        means.append(sum(sample) / len(sample))
    means.sort()
    return (sum(flat) / len(flat), means[int(0.025 * rounds)], means[int(0.975 * rounds)])


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", action="append", required=True, help="NAME=HISTORY:SNAPSHOT")
    parser.add_argument("--max-commits", type=int, default=1500)
    parser.add_argument("--tasks-per-repo", type=int, default=600)
    parser.add_argument("--budgets", default="1000,2000,4000,8000,16000")
    parser.add_argument("--seed", type=int, default=20260923)
    parser.add_argument("--out", type=Path, default=ROOT / "bench/results/context.jsonl")
    parser.add_argument("--fit-all", type=Path, help="also write the model fit on every repo, as .slop/relevance.json")
    args = parser.parse_args()
    budgets = [int(b) for b in args.budgets.split(",")]
    started = time.time()

    repos = []
    for spec in args.repo:
        name, paths = spec.split("=", 1)
        history, snapshot = paths.split(":", 1)
        repo = Repo(name, Path(history).expanduser(), Path(snapshot).expanduser(), budgets, args.max_commits)
        commits = sorted({task["commit"] for task in repo.tasks})
        random.Random(args.seed).shuffle(commits)
        kept, count = set(), 0
        for commit in commits:
            size = sum(task["commit"] == commit for task in repo.tasks)
            if count + size > args.tasks_per_repo:
                continue
            kept.add(commit)
            count += size
        repo.tasks = [task for task in repo.tasks if task["commit"] in kept]
        repo.run_slop(budgets)
        repos.append(repo)
        print(f"{name}: {len(repo.tasks)} tasks from {len(kept)} commits; catalog {len(repo.catalog)}")

    models = {}
    for held_out in repos:
        others = [repo for repo in repos if repo is not held_out] or [held_out]
        models[held_out.name] = fit(*training_rows(others, args.seed))

    # The shipped selector, scored out of sample: each repo's snapshot gets the model
    # fit without it, slop re-runs, and the file is removed again (scratch copies only).
    for repo in repos:
        model_file = repo.snapshot / ".slop/relevance.json"
        if model_file.exists():
            raise SystemExit(f"{model_file} exists; refusing to overwrite a repository's own model")
        model_file.parent.mkdir(exist_ok=True)
        model_file.write_text(json.dumps(model_json(models[repo.name], f"B4 leave-one-out, {repo.name} held out")))
        try:
            targets = sorted({task["target"] for task in repo.tasks})
            for selection in ("coverage", "ranked"):
                _, _, repo.picks[f"calibrated-{selection}"], repo.micros[f"calibrated-{selection}"] = slop_runs(
                    repo.snapshot, targets, budgets, selection)
        finally:
            model_file.unlink()
    if args.fit_all:
        pooled_model = fit(*training_rows(repos, args.seed))
        args.fit_all.write_text(json.dumps(model_json(pooled_model, "B4 pooled: " + ", ".join(
            f"{repo.name}@{git(repo.history, 'rev-parse', '--short', 'HEAD').strip()}" for repo in repos)), indent=2) + "\n")
        print(f"pooled model written to {args.fit_all}")

    arm_names = ["slop-coverage", "slop-ranked", "slop-calibrated-coverage", "slop-calibrated-ranked",
                 "calibrated-ratio", "calibrated-rank", "bm25", "proximity", "random"]
    arm_names += [f"adaptive-{t}" for t in THRESHOLDS] + ["oracle-pool", "oracle"]
    recall = {(arm, b, repo.name): defaultdict(list) for arm in arm_names for b in budgets for repo in repos}
    no_edge = {(arm, b, repo.name): defaultdict(list) for arm in arm_names for b in budgets for repo in repos}
    precision = {(arm, b, repo.name): defaultdict(list) for arm in arm_names for b in budgets for repo in repos}
    spent = {(arm, b, repo.name): defaultdict(list) for arm in arm_names for b in budgets for repo in repos}
    rng = random.Random(args.seed)
    for repo in repos:
        shuffled = sorted(repo.catalog)
        rng.shuffle(shuffled)
        beta = models[repo.name]
        for task in repo.tasks:
            target, gold = task["target"], set(task["gold"])
            neighbors, _ = repo.graph.get(target, (set(), {}))
            idents, features = repo.candidates(target)
            probability = 1 / (1 + np.exp(-features @ beta)) if len(idents) else np.array([])
            chance, pool = dict(zip(idents, probability)), set(idents)
            by_rank = [idents[i] for i in np.argsort(-probability, kind="stable")]
            by_ratio = [idents[i] for i in sorted(range(len(idents)),
                                                   key=lambda i: (-probability[i] / repo.cost[idents[i]], idents[i]))]
            lexical = repo.bm25.scores(repo.sources.get(target, []))
            bm25_order = [ident for ident, _ in sorted(lexical.items(), key=lambda kv: (-kv[1], kv[0])) if ident != target]
            entry = repo.catalog[target]
            directory = entry["file"].rsplit("/", 1)[0] if "/" in entry["file"] else ""
            near = sorted((i for i in repo.by_dir[directory] if i != target),
                          key=lambda i: (repo.catalog[i]["file"] != entry["file"],
                                         abs(repo.catalog[i]["lines"][0] - entry["lines"][0]), i))
            for budget in budgets:
                chosen = {
                    "slop-coverage": repo.picks["coverage"].get((target, budget), set()),
                    "slop-ranked": repo.picks["ranked"].get((target, budget), set()),
                    "slop-calibrated-coverage": repo.picks["calibrated-coverage"].get((target, budget), set()),
                    "slop-calibrated-ranked": repo.picks["calibrated-ranked"].get((target, budget), set()),
                    "calibrated-ratio": pack(by_ratio, repo.cost, budget),
                    "calibrated-rank": pack(by_rank, repo.cost, budget),
                    "bm25": pack(bm25_order, repo.cost, budget),
                    "proximity": pack(near, repo.cost, budget),
                    "random": pack((i for i in shuffled if i != target), repo.cost, budget),
                    # Ceilings: cheapest-first packing of the gold set maximises count recall.
                    "oracle-pool": pack(sorted(gold & pool, key=lambda i: (repo.cost[i], i)), repo.cost, budget),
                    "oracle": pack(sorted(gold, key=lambda i: (repo.cost[i], i)), repo.cost, budget),
                    **{f"adaptive-{t}": pack((i for i in by_ratio if chance[i] >= t), repo.cost, budget)
                       for t in THRESHOLDS},
                }
                far = gold - neighbors
                for arm, picked in chosen.items():
                    recall[(arm, budget, repo.name)][task["commit"]].append(len(gold & picked) / len(gold))
                    if picked:
                        precision[(arm, budget, repo.name)][task["commit"]].append(len(gold & picked) / len(picked))
                    spent[(arm, budget, repo.name)][task["commit"]].append(sum(repo.cost.get(i, 0) for i in picked))
                    if far:
                        no_edge[(arm, budget, repo.name)][task["commit"]].append(len(far & picked) / len(far))

    def pooled(table, arm, budget, names):
        merged = defaultdict(list)
        for name in names:
            for commit, values in table[(arm, budget, name)].items():
                merged[f"{name}:{commit}"].extend(values)
        return cluster_bootstrap(merged, args.seed)

    names = [repo.name for repo in repos]
    results = {}
    for arm in arm_names:
        for budget in budgets:
            for scope in names + ["pooled"]:
                members = names if scope == "pooled" else [scope]
                results[f"{arm}@{budget}@{scope}"] = {"recall": pooled(recall, arm, budget, members),
                                                      "no_edge": pooled(no_edge, arm, budget, members),
                                                      "precision": pooled(precision, arm, budget, members),
                                                      "tokens": pooled(spent, arm, budget, members)}

    print(f"\nelapsed {time.time() - started:.0f}s; models (leave-one-repo-out coefficients):")
    for name, beta in models.items():
        print(f"  held out {name}: " + ", ".join(f"{f}={b:+.2f}" for f, b in zip(FEATURES, beta)))
    for scope in ["pooled"] + names:
        tables = (("recall", "recall"), ("no-edge recall", "no_edge"))
        for label, key in tables + ((("precision", "precision"), ("tokens spent", "tokens")) if scope == "pooled" else ()):
            print(f"\n{scope} — {label}")
            print("| arm | " + " | ".join(f"@{b}" for b in budgets) + " |")
            print("| --- | " + " | ".join("---:" for _ in budgets) + " |")
            for arm in arm_names:
                cells = []
                for b in budgets:
                    mean, low, high = results[f"{arm}@{b}@{scope}"][key]
                    if key == "tokens":
                        cells.append(f"{mean:.0f}")
                    else:
                        cells.append(f"{mean:.1%} [{low:.0%}–{high:.0%}]" if scope == "pooled" else f"{mean:.1%}")
                print(f"| {arm} | " + " | ".join(cells) + " |")
    for repo in repos:
        for selection, values in repo.micros.items():
            values = sorted(values)
            print(f"{repo.name} slop-{selection}: p50 {values[len(values) // 2] / 1000:.1f} ms, "
                  f"p95 {values[int(0.95 * len(values))] / 1000:.1f} ms")

    args.out.parent.mkdir(parents=True, exist_ok=True)
    manifest = {
        "bench": "context", "label": "exploratory", "at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "commit": git(ROOT, "rev-parse", "HEAD").strip(),
        "dirty": bool(git(ROOT, "status", "--porcelain", "--untracked-files=no", "--", "crates", "bench",
                          ":!bench/__pycache__", ":!bench/results").strip()),
        "repos": {repo.name: {"history_head": git(repo.history, "rev-parse", "HEAD").strip(),
                              "snapshot": repo.header["snapshot"], "tasks": len(repo.tasks)} for repo in repos},
        "seed": args.seed, "budgets": budgets, "features": FEATURES, "python": platform.python_version(),
        "models": {name: list(map(float, beta)) for name, beta in models.items()},
        "tasks_digest": hashlib.sha256(json.dumps([r.tasks for r in repos], sort_keys=True).encode()).hexdigest(),
    }
    with args.out.open("a") as ledger:
        ledger.write(json.dumps({"manifest": manifest, "results": results}) + "\n")
    print(f"\nappended to {args.out}")


if __name__ == "__main__":
    main()
