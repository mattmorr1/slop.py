#!/usr/bin/env python3
"""B5: does binding a sketch surface the helpers the real implementation calls better than the envelope?

Pre-registered in bench/B5_PREREGISTRATION.md (gate G1). Three stages, R8's harness shape:

  prepare   R8's selection and gutting, then per task the envelope, bm25 and file lists and the gold
  generate  greedy sketches from a local Ollama, prompts `none` (R8's) and `sketch` (runs on the GPU host)
  score     binder v0 over every sketch, recall/precision at 5 and at budgets, paired bootstrap,
            appended to bench/results/bind.jsonl

  SLOP_BIN=target/release/slop python3 bench/b5_bench.py prepare \\
      --repo flask=PATH:25 --repo httpx=PATH:25 [--repo vigil=PATH:50] --r8-tasks R8/tasks.jsonl --out DIR
  python3 bench/b5_bench.py generate --tasks R8_OR_B5_TASKS.jsonl --model qwen3:8b --out DIR/sketches.jsonl
  SLOP_BIN=... python3 bench/b5_bench.py score --tasks DIR/tasks.jsonl --sketches DIR/sketches.jsonl
"""
from __future__ import annotations

import argparse
import hashlib
import json
import platform
import subprocess
import sys
import time
import urllib.request
from collections import Counter, defaultdict
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import reuse_bench as r8  # noqa: E402

ROOT = r8.ROOT
BUDGETS = (1000, 2000, 4000)
TOP = 5
LISTED = 50
PROMPTS = ("none", "sketch")
ARMS = ("bind", "envelope", "bm25", "file", "bind+file", "vocab-bm25")
PAIRS = (("bind", "envelope"), ("bind", "bm25"), ("bind", "file"), ("bind+file", "envelope"), ("bind", "vocab-bm25"))
VOCAB_CAP = 400
VOCAB_INTRO = "Names that already exist in this repository near this function:"
SKETCH_INSTRUCTION = ("Write this function as a sketch: call helpers by the names you would expect them to have, "
                      "and use `...` for parts you would look up. Reply with the complete function, signature "
                      "included, in a single ```python code block and nothing else.")


def prompt(task: dict, kind: str) -> str:
    none = task["prompts"]["none"]
    if r8.INSTRUCTION not in none:
        raise ValueError(f"{task['id']}: R8 instruction not found in its none prompt")
    if kind == "vocab":
        return task["prompts"]["vocab"]
    return none if kind == "none" else none.replace(r8.INSTRUCTION, SKETCH_INSTRUCTION)


def vocabulary(cat, target: str, file: str) -> tuple[list[str], dict[str, str]]:
    """B5b: kept names (alphabetical) and their entities, same file first then same directory under the token cap."""
    here = file.rsplit("/", 1)[0] if "/" in file else ""
    shown, tiers = {}, ([], [])
    for e, v in sorted(cat.entries.items()):
        parent = e.rsplit("::", 1)[0]
        nested = "()" in e.split("::", 1)[1] or (parent in cat.entries and not cat.entries[parent]["class"])
        if e == target or e.startswith(target + "::") or nested:
            continue
        if v["file"] == file or (v["file"].rsplit("/", 1)[0] if "/" in v["file"] else "") == here:
            shown[e] = e.split("::", 1)[1].replace("::", ".")
            tiers[v["file"] != file].append(shown[e])
    kept = []
    for name in dict.fromkeys(sorted(set(tiers[0])) + sorted(set(tiers[1]) - set(tiers[0]))):
        if r8.tokens(", ".join(kept + [name])) > VOCAB_CAP:
            break
        kept.append(name)
    return sorted(kept), {e: n for e, n in shown.items() if n in set(kept)}


def vocab(args) -> None:
    """B5b: add the vocabulary, its primed prompt and the vocab-bm25 control list to prepared tasks."""
    import b5_bind
    tasks = [json.loads(line) for line in args.tasks.read_text().splitlines()]
    for copy in sorted({t["copy"] for t in tasks}):
        cat = b5_bind.Catalog(Path(copy))
        for t in (t for t in tasks if t["copy"] == copy):
            names, entities = vocabulary(cat, t["target"], t["file"])
            block = f"{VOCAB_INTRO}\n{', '.join(names) or '(none)'}\n"
            none = t["prompts"]["none"]
            if none.count(f"\n{r8.INSTRUCTION}") != 1:
                raise ValueError(f"{t['id']}: R8 instruction not found once in its none prompt")
            entry = cat.entries[t["target"]]
            stub = [line for line in cat.lines[t["file"]][entry["start"]: entry["end"] + 1] if "raise NotImplementedError" not in line]
            scores = r8.Bm25({e: b5_bind.words(n) for e, n in entities.items()}).scores(b5_bind.words("\n".join(stub)))
            t["vocab"] = names
            t["prompts"]["vocab"] = none.replace(f"\n{r8.INSTRUCTION}", f"\n{block}\n{r8.INSTRUCTION}")
            t["vocab_bm25"] = [e for _, e in sorted((-v, e) for e, v in scores.items() if v > 0)][:LISTED]
    args.out.write_text("".join(json.dumps(t) + "\n" for t in tasks))
    sizes = sorted(len(t["vocab"]) for t in tasks)
    print(f"{len(tasks)} tasks -> {args.out} (sha256 {hashlib.sha256(args.out.read_bytes()).hexdigest()}); "
          f"names per task min {sizes[0]} median {sizes[len(sizes) // 2]} max {sizes[-1]}; "
          f"{sum(r8.tokens(', '.join(t['vocab'])) >= VOCAB_CAP - 10 for t in tasks)} at the cap")


def git_head(path: Path) -> str:
    out = subprocess.run(["git", "-C", str(path), "rev-parse", "HEAD"], capture_output=True, text=True)
    return out.stdout.strip() if out.returncode == 0 else "unknown"


def prepare(args) -> None:
    import b5_bind
    r8_dir = args.out / "r8"
    archived = {t["id"]: t for t in map(json.loads, args.r8_tasks.read_text().splitlines())} if args.r8_tasks else {}
    if args.pin and not archived:
        raise SystemExit("--pin needs --r8-tasks")
    pinned = {repo: [i.split(":", 1)[1] for i, t in archived.items() if t["repo"] == repo] for repo in args.pin}
    r8.prepare(argparse.Namespace(repo=args.repo, out=r8_dir, budget=4000, pinned=pinned))
    prepared = [json.loads(line) for line in (r8_dir / "tasks.jsonl").read_text().splitlines()]
    same_prompt = lambda t: t["id"] in archived and archived[t["id"]]["prompts"]["none"] == t["prompts"]["none"]
    for repo in args.pin:
        mine = [t for t in prepared if t["repo"] == repo]
        print(f"{repo} pinned: {len(mine)} of {len(pinned[repo])} archived targets exist, {sum(map(same_prompt, mine))} with the archived "
              f"prompt (kept), {sum(same_prompt(t) and archived[t['id']]['callees'] == t['callees'] for t in mine)} of those with the archived callees",
              file=sys.stderr)
    prepared = [t for t in prepared if t["repo"] not in args.pin or same_prompt(t)]
    if archived:
        bad = [t["id"] for t in prepared if t["repo"] not in args.pin and not (same_prompt(t) and archived[t["id"]]["callees"] == t["callees"])]
        if bad:
            raise SystemExit(f"{len(bad)} tasks differ from R8's archive, e.g. {bad[:3]}")
    sources = {spec.split("=", 1)[0]: Path(spec.split("=", 1)[1].rsplit(":", 1)[0]).expanduser().resolve() for spec in args.repo}
    tasks = []
    for repo, source in sources.items():
        copy = r8_dir / f"{repo}-gutted"
        cat = b5_bind.Catalog(copy)
        mine = [t for t in prepared if t["repo"] == repo]
        rows = r8.slop_bench(copy, [t["target"] for t in mine], ",".join(map(str, BUDGETS)), ["--min-probability-ppm", str(r8.ADAPTIVE_PPM)])
        envelopes = {(row["target"], row["budget"]): [[i["entity"], i["score"], i["tokens"]] for i in row["items"] if i["entity"] != row["target"]]
                     for row in rows if "items" in row}
        skeletons = {e: r8.skeleton(cat.lines[cat.entries[e]["file"]], cat.nodes[e]) for e in cat.entries}
        index = r8.Bm25({e: r8.WORD.findall(text.lower()) for e, text in skeletons.items()})
        for t in mine:
            entry = cat.entries[t["target"]]
            stub = "\n".join(cat.lines[t["file"]][entry["start"]: entry["end"] + 1])
            bm25 = [e for _, e in sorted((-v, e) for e, v in index.scores(r8.WORD.findall(stub.lower())).items() if e != t["target"])]
            same = sorted((abs(v["start"] - entry["start"]), e) for e, v in cat.entries.items() if v["file"] == t["file"] and e != t["target"])
            tasks.append({"id": t["id"], "repo": repo, "target": t["target"], "name": t["name"], "file": t["file"],
                          "copy": str(copy), "source_commit": git_head(source), "gold": t["callees"],
                          "gold_cross": [c for c in t["callees"] if cat.files.get(c, t["file"]) != t["file"]],
                          "prompts": {kind: prompt(t, kind) for kind in PROMPTS},
                          "envelope": {str(b): envelopes[(t["target"], b)] for b in BUDGETS},
                          "bm25": bm25[:LISTED], "file_rank": [e for _, e in same[:LISTED]]})
        print(f"{repo}: {len(mine)} tasks, source {git_head(source)[:12]}", file=sys.stderr)
    (args.out / "tasks.jsonl").write_text("".join(json.dumps(t) + "\n" for t in tasks))
    print(f"{len(tasks)} tasks -> {args.out / 'tasks.jsonl'} (sha256 {hashlib.sha256((args.out / 'tasks.jsonl').read_bytes()).hexdigest()})")


def measure(ranked: list[str], cost, gold: list[str], cross: list[str]) -> dict:
    """Recall and precision over the top 5, cross-file recall@5, and recall and tokens packed into each budget."""
    top, gold_set = ranked[:TOP], set(gold)
    hits = sum(e in gold_set for e in top)
    row = {"recall5": hits / len(gold), "precision5": hits / len(top) if top else 0.0,
           "cross5": sum(e in top for e in cross) / len(cross) if cross else None}
    for budget in BUDGETS:
        kept, spent = set(), 0
        for entity in ranked:
            if spent + cost(entity) <= budget:
                kept.add(entity)
                spent += cost(entity)
        row[f"recall@{budget}"], row[f"tokens@{budget}"] = len(gold_set & kept) / len(gold), spent
    return row


def envelope_measure(task: dict) -> dict:
    by_score = [e for e, _, _ in sorted(task["envelope"]["4000"], key=lambda i: (-i[1], i[0]))]
    row = measure(by_score[:TOP], lambda e: 0, task["gold"], task["gold_cross"])
    for budget in BUDGETS:
        items = task["envelope"][str(budget)]
        row[f"recall@{budget}"] = len(set(task["gold"]) & {e for e, _, _ in items}) / len(task["gold"])
        row[f"tokens@{budget}"] = sum(n for _, _, n in items)
    return row


def mean(values: list) -> float | None:
    present = [v for v in values if v is not None]
    return sum(present) / len(present) if present else None


def percentile(values: list[float], q: float) -> float:
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, int(q * len(ordered)))] if ordered else float("nan")


def bind_all(tasks: dict, sketches: list[dict]) -> tuple[dict, list, dict]:
    """Every sketch through binder v0, one warm MCP process per repository."""
    import b5_bind
    per_task, per_sketch, trace = {}, [], []
    stats = {"latency": [], "esound": 0, "parse_errors": Counter(), "calls": []}
    for repo in sorted({t["repo"] for t in tasks.values()}):
        binder = b5_bind.Binder(Path(next(t["copy"] for t in tasks.values() if t["repo"] == repo)))
        cost, warm = binder.catalog.cost, False
        for t in (t for t in tasks.values() if t["repo"] == repo):
            per_task[t["id"]] = {"envelope": envelope_measure(t), "bm25": measure(t["bm25"], cost, t["gold"], t["gold_cross"]),
                                 "file": measure(t["file_rank"], cost, t["gold"], t["gold_cross"])}
            if "vocab_bm25" in t:
                per_task[t["id"]]["vocab-bm25"] = measure(t["vocab_bm25"], cost, t["gold"], t["gold_cross"])
        for s in (s for s in sketches if tasks[s["id"]]["repo"] == repo):
            t = tasks[s["id"]]
            node, ranked = r8.generated_function(s["response"], t["name"]), []
            if node is None:
                stats["parse_errors"][f"{s['prompt']}/{s['model']}"] += 1
            else:
                if not warm:
                    binder.bind(node, t["file"], t["target"])
                    warm = True
                result = binder.bind(node, t["file"], t["target"])
                ranked = result["ranked"]
                stats["esound"] += result["esound"]
                stats["latency"].append(result["seconds"])
                cat, vocab_names = binder.catalog, {n.rsplit(".", 1)[-1] for n in t.get("vocab", [])}
                imported = b5_bind.imported_names(cat.tree(t["file"]))
                buckets = Counter("vocab" if n in vocab_names else "other" if cat.by_name.get(n) or cat.by_norm.get(b5_bind.normalised(n))
                                  else "imported" if n in imported else "invented" for n in result["called"])
                stats["calls"].append({"repo": repo, "prompt": s["prompt"], **buckets})
                trace.append({"id": s["id"], "prompt": s["prompt"], "model": s["model"], "top": ranked[:20], "via": result["via"],
                              "calls": result["calls"], "unresolved": result["unresolved"], "ms": round(result["seconds"] * 1000, 2)})
            per_sketch.append({"id": s["id"], "prompt": s["prompt"], "model": s["model"],
                               "bind": measure(ranked, cost, t["gold"], t["gold_cross"]),
                               "bind+file": measure(list(dict.fromkeys(t["file_rank"] + ranked)), cost, t["gold"], t["gold_cross"])})
        binder.mcp.close()
    return per_task, per_sketch, {**stats, "trace": trace}


def arms(per_task: dict) -> list[str]:
    return [a for a in ARMS if a in ("bind", "bind+file") or a in next(iter(per_task.values()))]


def pairs(per_task: dict) -> list[tuple[str, str]]:
    return [(a, b) for a, b in PAIRS if b in arms(per_task)]


def call_stats(calls: list[dict]) -> dict:
    """Mean candidate calls per parsed sketch and their split into vocabulary / other repo / imported / invented."""
    out = {}
    for label, rows in [("pooled", calls)] + [(f"repo={r}", [c for c in calls if c["repo"] == r]) for r in sorted({c["repo"] for c in calls})]:
        total = {k: sum(c.get(k, 0) for c in rows) for k in ("vocab", "other", "imported", "invented")}
        n = sum(total.values())
        out[label] = {"sketches": len(rows), "calls_per_sketch": n / len(rows), "resolved_per_sketch": (total["vocab"] + total["other"]) / len(rows),
                      **{f"share_{k}": v / n if n else None for k, v in total.items()}}
    return out


def summarise(tasks: dict, per_task: dict, per_sketch: list[dict]) -> dict:
    metrics = ["recall5", "precision5", "cross5"] + [f"recall@{b}" for b in BUDGETS]
    scopes = [("pooled", None, None)] + [(f"repo={r}", "repo", r) for r in sorted({t["repo"] for t in tasks.values()})] \
        + [(f"prompt={p}", "prompt", p) for p in sorted({s["prompt"] for s in per_sketch})] \
        + [(f"model={m}", "model", m) for m in sorted({s["model"] for s in per_sketch})]
    results = {}
    for label, key, want in scopes:
        by_task = defaultdict(list)
        for s in per_sketch:
            if key is None or (tasks[s["id"]]["repo"] if key == "repo" else s[key]) == want:
                by_task[s["id"]].append(s)
        if not by_task:
            continue
        # Cluster = task: a task's sketches are averaged first, then tasks are resampled.
        values = {i: {**per_task[i], **{arm: {m: mean([s[arm][m] for s in rows]) for m in metrics + [f"tokens@{b}" for b in BUDGETS]}
                                        for arm in ("bind", "bind+file")}} for i, rows in sorted(by_task.items())}
        entry = {"tasks": len(values), "sketches": sum(map(len, by_task.values()))}
        for arm in arms(per_task):
            entry[arm] = {m: r8.bootstrap([v[arm][m] for v in values.values() if v[arm][m] is not None]) for m in metrics}
            entry[arm].update({f"tokens@{b}": mean([v[arm][f"tokens@{b}"] for v in values.values()]) for b in BUDGETS})
        for a, b in pairs(per_task):
            for m in metrics:
                entry[f"{a}_minus_{b}_{m}"] = r8.bootstrap([v[a][m] - v[b][m] for v in values.values() if v[a][m] is not None and v[b][m] is not None])
        results[label] = entry
    return results


def gate(results: dict) -> dict:
    def one(label: str) -> dict:
        if label not in results:
            return {"verdict": "not scored"}
        diff, precision = results[label]["bind_minus_envelope_recall5"], results[label]["bind"]["precision5"][0]
        ok, row = diff[0] >= 0.10 and diff[1] > 0 and precision >= 0.30, {"recall5_diff": diff, "precision5": precision}
        if "bind_minus_vocab-bm25_recall5" in results[label]:
            control = row["vocab_bm25_recall5_diff"] = results[label]["bind_minus_vocab-bm25_recall5"]
            ok = ok and control[0] > 0 and control[1] > 0
        return {"verdict": "pass" if ok else "fail", **row}
    g1 = {"pooled": one("pooled"), "vigil": one("repo=vigil")}
    verdicts = {g1["pooled"]["verdict"], g1["vigil"]["verdict"]}
    g1["verdict"] = ("undecided: vigil not scored" if "repo=vigil" not in results else
                     "pass" if verdicts == {"pass"} else "fail" if verdicts == {"fail"} else "mixed")
    return g1


def score(args) -> None:
    tasks = {t["id"]: t for t in map(json.loads, args.tasks.read_text().splitlines())}
    sketches = [json.loads(line) for path in args.sketches for line in path.read_text().splitlines()]
    unscored = Counter(s["id"].split(":", 1)[0] for s in sketches if s["id"] not in tasks)
    per_task, per_sketch, stats = bind_all(tasks, [s for s in sketches if s["id"] in tasks])
    primed = "vocab-bm25" in next(iter(per_task.values()))
    if args.trace:
        args.trace.write_text("".join(json.dumps(row) + "\n" for row in stats["trace"]))
    results = summarise(tasks, per_task, per_sketch)
    lat = stats["latency"]
    extra = {"latency_ms": {"p50": percentile(lat, 0.5) * 1000, "p95": percentile(lat, 0.95) * 1000, "n": len(lat)},
             "esound_fires": stats["esound"], "parse_errors": dict(sorted(stats["parse_errors"].items())),
             "unscored_sketches": dict(unscored), "calls": call_stats(stats["calls"]), ("g1b" if primed else "g1"): gate(results)}

    pct = lambda e: f"{e[0] * 100:5.1f}% [{e[1] * 100:.1f}, {e[2] * 100:.1f}]"
    diff = lambda e: f"{e[0] * 100:+5.1f} [{e[1] * 100:+.1f}, {e[2] * 100:+.1f}]"
    for label, entry in results.items():
        print(f"\n{label} ({entry['tasks']} tasks, {entry['sketches']} sketches)")
        print("| arm | recall@5 | precision@5 | cross-file recall@5 | " + " | ".join(f"recall@{b // 1000}k (tokens)" for b in BUDGETS) + " |")
        print("| --- |" + " ---: |" * (3 + len(BUDGETS)))
        for arm in (a for a in ARMS if a in entry):
            x = entry[arm]
            print(f"| {arm} | {pct(x['recall5'])} | {pct(x['precision5'])} | {pct(x['cross5'])} | "
                  + " | ".join(f"{x[f'recall@{b}'][0] * 100:.1f}% ({x[f'tokens@{b}']:.0f})" for b in BUDGETS) + " |")
        for a, b in pairs(per_task):
            print(f"{a} - {b}: recall@5 {diff(entry[f'{a}_minus_{b}_recall5'])}, precision@5 {diff(entry[f'{a}_minus_{b}_precision5'])}, "
                  f"cross@5 {diff(entry[f'{a}_minus_{b}_cross5'])}, " + ", ".join(f"@{x // 1000}k {diff(entry[f'{a}_minus_{b}_recall@{x}'])}" for x in BUDGETS))
    print(f"\n{json.dumps(extra, indent=1)}")
    binary = Path(r8.SLOP).resolve()
    status = subprocess.run(["git", "-C", str(ROOT), "status", "--porcelain"], capture_output=True, text=True, check=True).stdout
    manifest = {"bench": "bind", "label": f"pre-registered (bench/{'B5B' if primed else 'B5'}_PREREGISTRATION.md)", "at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
                "commit": git_head(ROOT), "dirty": bool(status.strip()), "seed": r8.SEED,
                "tasks_digest": hashlib.sha256(args.tasks.read_bytes()).hexdigest(),
                "sketches_digest": hashlib.sha256(b"".join(p.read_bytes() for p in args.sketches)).hexdigest(),
                "source_commits": {t["repo"]: t["source_commit"] for t in tasks.values()},
                "binary": {"path": str(binary), "sha256": hashlib.sha256(binary.read_bytes()).hexdigest(), "built_from": args.binary_commit},
                "models": sorted({s["model"] for s in per_sketch}), "prompts": sorted({s["prompt"] for s in per_sketch}), "budgets": list(BUDGETS),
                "python": platform.python_version(), "platform": platform.platform()}
    ledger = ROOT / "bench/results/bind.jsonl"
    with ledger.open("a") as handle:
        handle.write(json.dumps({"manifest": manifest, "results": results, **extra}) + "\n")
    print(f"\nappended to {ledger}")


def dupes(args) -> None:
    """Secondary check: each known copy as the sketch (written to a new file), are the other copies in bind's top 5?"""
    import b5_bind
    binder = b5_bind.Binder(args.repo.resolve())
    cat, rows = binder.catalog, []
    for spec in args.group:
        label, members = spec.split("=", 1)
        entities = [next(e for e, v in cat.entries.items() if v["file"] == f and v["name"] == n)
                    for f, n in (m.rsplit(":", 1) for m in members.split(","))]
        for entity in entities:
            file = cat.entries[entity]["file"]
            result = binder.bind(cat.nodes[entity], file if args.in_place else f"{file.rsplit('/', 1)[0]}/b5_sketch.py", entity)
            ranks = {o: result["ranked"].index(o) + 1 if o in result["ranked"] else None for o in entities if o != entity}
            rows.append({"group": label, "sketch": entity, "others_in_top5": sum(r is not None and r <= TOP for r in ranks.values()),
                         "others": len(ranks), "ranks": ranks, "esound": result["esound"]})
            print(f"{label}: {entity} -> {rows[-1]['others_in_top5']}/{len(ranks)} in top 5, ranks {list(ranks.values())}, E-sound {result['esound']}")
    binder.mcp.close()
    hits = sum(r["others_in_top5"] for r in rows)
    print(f"{hits}/{sum(r['others'] for r in rows)} other copies in the top 5")


def generate(args) -> None:
    done = set()
    if args.out.exists():
        done = {(r["id"], r["prompt"], r["model"]) for r in map(json.loads, args.out.read_text().splitlines())}
    tasks = [json.loads(line) for line in args.tasks.read_text().splitlines()]
    with args.out.open("a") as out:
        if args.reuse:
            # R8 run 1 rows for the identical `none` prompts (tasks digest checked by the caller, see pre-registration).
            for row in map(json.loads, args.reuse.read_text().splitlines()):
                if row["arm"] == "none" and row["model"] == args.model and (row["id"], "none", args.model) not in done:
                    out.write(json.dumps({**{k: v for k, v in row.items() if k != "arm"}, "prompt": "none", "reused": "R8 run 1"}) + "\n")
                    done.add((row["id"], "none", args.model))
        todo = [(t, kind) for t in tasks for kind in args.prompts.split(",") if (t["id"], kind, args.model) not in done]
        for n, (task, kind) in enumerate(todo):
            body = json.dumps({"model": args.model, "prompt": prompt(task, kind), "stream": False, "think": False,
                               "options": {"temperature": 0, "seed": 0, "num_ctx": args.num_ctx, "num_predict": 768}})
            request = urllib.request.Request(f"{args.host}/api/generate", data=body.encode(),
                                             headers={"Content-Type": "application/json"})
            started = time.time()
            with urllib.request.urlopen(request, timeout=600) as response:
                reply = json.loads(response.read())
            out.write(json.dumps({"id": task["id"], "prompt": kind, "model": args.model, "response": reply["response"],
                                  "prompt_eval_count": reply.get("prompt_eval_count"),
                                  "eval_count": reply.get("eval_count"), "seconds": round(time.time() - started, 2)}) + "\n")
            out.flush()
            print(f"{n + 1}/{len(todo)} {args.model} {kind} {task['id']} {time.time() - started:.1f}s", file=sys.stderr, flush=True)


def main() -> None:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="stage", required=True)
    p = sub.add_parser("prepare")
    p.add_argument("--repo", action="append", required=True, help="NAME=PATH:COUNT")
    p.add_argument("--out", type=Path, required=True)
    p.add_argument("--r8-tasks", type=Path, help="R8's archived tasks.jsonl; prepared tasks must match it")
    p.add_argument("--pin", action="append", default=[], help="repo whose targets are R8's archived ids instead of a fresh sample")
    g = sub.add_parser("generate")
    g.add_argument("--tasks", type=Path, required=True)
    g.add_argument("--model", required=True)
    g.add_argument("--out", type=Path, required=True)
    g.add_argument("--prompts", default=",".join(PROMPTS))
    g.add_argument("--reuse", type=Path, help="R8 out.jsonl whose `none` rows for --model are imported")
    g.add_argument("--host", default="http://127.0.0.1:11434")
    g.add_argument("--num-ctx", type=int, default=8192)
    s = sub.add_parser("score")
    s.add_argument("--tasks", type=Path, required=True)
    s.add_argument("--sketches", type=Path, nargs="+", required=True)
    s.add_argument("--trace", type=Path, help="per-sketch bindings (top 20, signals, latency) for replay")
    s.add_argument("--binary-commit", default="278c7fb")
    v = sub.add_parser("vocab")
    v.add_argument("--tasks", type=Path, required=True, help="tasks.jsonl from prepare")
    v.add_argument("--out", type=Path, required=True)
    d = sub.add_parser("dupes")
    d.add_argument("--repo", type=Path, required=True)
    d.add_argument("--in-place", action="store_true", help="destination is the copy's own file, not a new file beside it")
    d.add_argument("--group", action="append", required=True, help="LABEL=file:name,file:name,...")
    args = parser.parse_args()
    {"prepare": prepare, "generate": generate, "score": score, "dupes": dupes, "vocab": vocab}[args.stage](args)


if __name__ == "__main__":
    main()
