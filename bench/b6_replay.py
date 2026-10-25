#!/usr/bin/env python3
"""B6: replay slop's Stop gate over Claude-written commits (pre-registered in B6_PREREGISTRATION.md).

  python3 bench/b6_replay.py run --repo NAME=PATH [...] --work /tmp/b6 [--list]
  python3 bench/b6_replay.py score [--ledger bench/results/replay.jsonl] [--labels bench/results/b6_labels.jsonl]
"""
from collections import Counter
import argparse, hashlib, json, os, platform, random, re, select, shutil, subprocess, sys, time, uuid
from pathlib import Path

SEED, CAP = 20261025, 60
ROOT = Path(__file__).resolve().parent.parent
BIN = Path(os.environ.get("SLOP_BIN", ROOT / "target/release/slop"))
LEDGER, LABELS = ROOT / "bench/results/replay.jsonl", ROOT / "bench/results/b6_labels.jsonl"
INDEX_TIMEOUT, HOOK_TIMEOUT, MCP_TIMEOUT = 1200, 300, 600
PARTIAL, BENCH = False, "B6"
REFUSED = "refusing to persist prewrite evidence from stale snapshot"
ADV = re.compile(r"^`[^`]*` \(([^:()]+):(\d+)\) (has the same shape as|overlaps existing) `[^`]*` \(([^:()]+):(\d+)\)")


def sh(args, cwd=None, timeout=None, stdin=None, check=True):
    p = subprocess.run(args, cwd=cwd, input=stdin, capture_output=True, text=True, timeout=timeout)
    if check and p.returncode:
        raise RuntimeError(f"{' '.join(map(str, args[:3]))} exited {p.returncode}: {p.stderr.strip()[-400:]}")
    return p


def git(repo, *args, check=True):
    return sh(["git", "-C", str(repo), *args], check=check).stdout


def units(repo):
    """Claude co-authored commits with one parent that touch a .py file, in a seeded sample of at most CAP."""
    rows = [l.split() for l in git(repo, "log", "--all", "-i", "--grep=Co-Authored-By: Claude", "--format=%H %P").splitlines() if l]
    one = sorted(r for r in rows if len(r) == 2)
    py = [(c, p) for c, p in one if any(f.endswith(".py") for f in git(repo, "diff", "--name-only", "--no-renames", p, c).split("\n"))]
    sample = sorted(random.Random(SEED).sample(py, CAP)) if len(py) > CAP else py
    return len(rows), len(one), len(py), sample


def hook(repo, event, payload):
    at = time.perf_counter()
    p = sh([str(BIN), "hook", event], cwd=repo, stdin=json.dumps(payload), timeout=HOOK_TIMEOUT, check=False)
    ms = (time.perf_counter() - at) * 1000
    out = json.loads(p.stdout) if p.returncode == 0 and p.stdout.strip() else None
    return ms, out, p.returncode, p.stderr.strip()[-400:]


RACE = "an index was rewritten while capturing analysis"


def mcp_reconcile(repo, session_id, work):
    """reconcile over one live `slop mcp`; retries only the index-rewrite race, as its message asks, and counts retries."""
    err = open(work / "mcp.stderr", "w+")
    p = subprocess.Popen([str(BIN), "mcp", str(repo)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=err, text=True)
    deadline = time.monotonic() + MCP_TIMEOUT
    def call(i, method, params):
        p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": i, "method": method, "params": params}) + "\n")
        p.stdin.flush()
        while select.select([p.stdout], [], [], max(0, deadline - time.monotonic()))[0]:
            line = p.stdout.readline()
            if not line:
                break
            if json.loads(line).get("id") == i:
                return json.loads(line)
        err.seek(0)
        raise RuntimeError(f"mcp gave no reply to {method} (rc {p.poll()}): {err.read().strip()[-300:]}")
    try:
        call(1, "initialize", {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "b6", "version": "1"}})
        p.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
        at, retries = time.perf_counter(), 0
        for i in range(2, 200):
            reply = call(i, "tools/call", {"name": "reconcile", "arguments": {"session_id": session_id}})
            text = reply.get("result", {}).get("content", [{}])[0].get("text", "")
            if reply.get("result", {}).get("isError") and RACE in text and time.monotonic() < deadline:
                retries += 1
                time.sleep(2)
                continue
            if "error" in reply or reply["result"].get("isError"):
                raise RuntimeError(f"reconcile failed: {json.dumps(reply)[:300]}")
            return (time.perf_counter() - at) * 1000, retries, json.loads(text)
        raise RuntimeError(f"reconcile still racing after {retries} retries")
    finally:
        p.kill()
        p.wait()
        err.close()


def later_touch(repo, commit, file, line, end):
    """Weak 'fixed later' signal: is the flagged line's exact text still in the file at the branch tip?"""
    before = sh(["git", "-C", str(repo), "show", f"{commit}:{file}"], check=False).stdout.splitlines()
    if not file or not 1 <= line <= len(before):
        return "unknown"
    if end == commit:
        return "no-later-commit"
    after = sh(["git", "-C", str(repo), "show", f"{end}:{file}"], check=False)
    if after.returncode:
        return "file-removed"
    return "line-present" if before[line - 1].strip() in {l.strip() for l in after.stdout.splitlines()} else "line-changed"


def tip(repo, commit):
    """The branch tip that contains `commit` (HEAD if it does), so 'later' follows the commit's own line."""
    head = git(repo, "rev-parse", "refs/remotes/origin/HEAD").strip()
    if sh(["git", "-C", str(repo), "merge-base", "--is-ancestor", commit, head], check=False).returncode == 0:
        return head
    tips = git(repo, "for-each-ref", "--contains", commit, "--format=%(objectname)", "refs/remotes").split()
    return tips[0] if tips else commit


def rule(fid):
    return fid.split(":", 1)[0]


def replay(repo, name, commit, parent, py_files, index_cache=None, reuse=False):
    u = {"repo": name, "commit": commit, "parent": parent, "py_files": py_files, "errors": []}
    hooks_log = repo / ".slop/hooks.jsonl"
    git(repo, "checkout", "-q", "-f", "--detach", parent)
    git(repo, "clean", "-ffdxq")
    tmp = Path(os.environ["TMPDIR"])
    for stale in tmp.glob("slop-prewrite-*.json"):
        stale.unlink()
    at = time.perf_counter()
    try:
        # B7 arm B restores arm A's index of the same parent (reconcile reindexes at the commit); stamps are content digests.
        if reuse:
            shutil.copytree(index_cache, repo / ".slop/index")
        else:
            sh([str(BIN), "index", str(repo)], timeout=INDEX_TIMEOUT)
            if index_cache:
                shutil.rmtree(index_cache, ignore_errors=True)
                shutil.copytree(repo / ".slop/index", index_cache)
        u["index"] = {"ok": True, "s": round(time.perf_counter() - at, 1), **({"reused": True} if reuse else {})}
    except (RuntimeError, subprocess.TimeoutExpired) as e:
        u["index"] = {"ok": False, "s": round(time.perf_counter() - at, 1), "error": str(e)[-400:]}
        # Exploratory only (pre-registration change 3): go on when the Python artifact was written.
        u["partial_index"] = PARTIAL and (repo / ".slop/index/python.scip").is_file()
        if not u["partial_index"]:
            u["scorable"] = False
            return u
    if (repo / "slop.toml").exists():
        u["policy"] = "committed slop.toml"
    else:
        p = sh([str(BIN), "init", str(repo)], timeout=INDEX_TIMEOUT, check=False)
        proposed = len(re.findall(r"^\s*\w+\s*=", p.stdout, re.M)) if "proposed policy" in p.stdout else 0
        u["policy"] = f"none (init proposed {proposed} channel key(s), unconfirmed: not used)" if p.returncode == 0 else f"none (init failed: {p.stderr.strip()[-200:]})"
    sid = f"{BENCH.lower()}-{commit[:12]}-{uuid.uuid4().hex[:8]}"
    payload = lambda event, extra: {"session_id": sid, "transcript_path": "/dev/null", "cwd": str(repo), "hook_event_name": event, **extra}
    ms, out, rc, err = hook(repo, "session-start", payload("SessionStart", {"source": "startup"}))
    u["start_ms"], u["sidecar"] = round(ms, 1), any(tmp.glob("slop-prewrite-*.json"))
    if rc or out is None:
        u["errors"].append(f"session-start rc {rc}: {err}")
    git(repo, "checkout", "-q", "--detach", commit)
    ms, out, rc, err = hook(repo, "stop", payload("Stop", {"permission_mode": "default", "stop_hook_active": False}))
    u["stop_ms"] = round(ms, 1)
    if rc or out is None:
        u["errors"].append(f"stop rc {rc}: {err}")
    u["blocked"] = bool(out and out.get("decision") == "block")
    u["stop_ids"] = re.findall(r"^\d+\. \[([^\]]+)\]", out.get("reason", ""), re.M) if u["blocked"] else []
    if BENCH == "B7":
        u["stop_reason"] = out.get("reason", "") if u["blocked"] else ""
    u["hook_errors"] = [json.loads(l)["error"][-300:] for l in hooks_log.read_text().splitlines() if '"error"' in l] if hooks_log.is_file() else []
    try:
        ms, u["reconcile_retries"], rep = mcp_reconcile(repo, sid, tmp)
    except (RuntimeError, subprocess.TimeoutExpired) as e:
        u["errors"].append(f"reconcile: {str(e)[-300:]}")
        u["scorable"] = not u["errors"] and not fatal(u)
        return u
    end = tip(repo, commit)
    u["reconcile_ms"], u["changed_files"], u["sidecar_reconcile"] = round(ms, 1), len(rep["changed_files"]), rep["sidecar"] == "loaded"
    u["findings"] = [{"id": f["id"], "rule": rule(f["id"]), "file": f["file"], "line": f["line"], "later_touched": later_touch(repo, commit, f["file"], f["line"], end), **({"fact": f["fact"], "action": f["action"]} if BENCH == "B7" else {})} for f in rep["provable"]]
    u["advisory"] = []
    for n, text in enumerate(rep["advisory"]):
        m = ADV.match(text)
        if not m:
            u["errors"].append(f"advisory {n} did not parse")
            continue
        f, l, kind, of, ol = m.groups()
        u["advisory"].append({"id": f"adv:{commit[:12]}:{n}", "kind": "shape" if kind.startswith("has") else "overlap", "file": f, "line": int(l), "existing": f"{of}:{ol}", "later_touched": later_touch(repo, commit, f, int(l), end), **({"text": text} if BENCH == "B7" else {})})
    u["detectors"] = len(rep["detectors"].get("findings") or [])
    if not consistent(u):
        u["errors"].append(f"stop ids {u['stop_ids']} != reconcile provable {[f['id'] for f in u['findings']]}")
    u["scorable"] = not u["errors"] and not fatal(u)
    return u


def fatal(u):
    """Hook errors that void a unit; under a partial index the sidecar refusal is expected and only recorded."""
    return [e for e in u.get("hook_errors", []) if not (u.get("partial_index") and e.startswith(REFUSED))]


def consistent(u):
    """Stop listed min(8, n) of reconcile's n findings, as a multiset: two copies of one function share an id."""
    stop, found = Counter(u["stop_ids"]), Counter(f["id"] for f in u["findings"])
    return not stop - found and sum(stop.values()) == min(8, sum(found.values()))


def scorable(u):
    """The run-time check compared sets, which fails on a repeated id; recompute it as a multiset."""
    if u.get("scorable"):
        return True
    return "findings" in u and not fatal(u) and consistent(u) and all(e.startswith("stop ids ") for e in u["errors"])


def manifest(repos, seed_counts):
    sha = sh(["git", "-C", str(ROOT), "rev-parse", "HEAD"]).stdout.strip()
    crates = sh(["git", "-C", str(ROOT), "log", "-1", "--format=%H", "--", "crates", "Cargo.toml", "Cargo.lock"]).stdout.strip()
    dirty = bool(sh(["git", "-C", str(ROOT), "status", "--porcelain", "--", "crates"]).stdout.strip())
    return {"bench": "B6", "slop_sha": sha, "crates_sha": crates, "dirty": dirty, "binary": str(BIN), "seed": SEED, "cap": CAP, "platform": platform.platform(),
            "python": platform.python_version(), "repos": {n: {"head": git(r, "rev-parse", "refs/remotes/origin/HEAD").strip(), **seed_counts[n]} for n, r in repos.items()}}


def run(a):
    repos = {}
    for spec in a.repo:
        name, src = spec.split("=", 1)
        dst = Path(a.work) / "repos" / name
        if not dst.exists():
            sh(["git", "clone", "-q", "--no-hardlinks", src, str(dst)])
            # A local clone copies only branches; the source's remote-tracking branches and tags hold Claude commits too.
            git(dst, "fetch", "-q", src, "+refs/heads/*:refs/remotes/src-heads/*", "+refs/remotes/*:refs/remotes/src/*", "+refs/tags/*:refs/tags/*")
        repos[name] = dst.resolve()
    # Each run owns its TMPDIR, so the prewrite sidecar (std::env::temp_dir) is ours to clear per unit.
    os.environ["TMPDIR"] = str(Path(a.work) / "tmp" / "-".join(sorted(repos)))
    Path(os.environ["TMPDIR"]).mkdir(parents=True, exist_ok=True)
    counts, plan = {}, []
    for name, repo in repos.items():
        n_all, n_one, n_py, sample = units(repo)
        counts[name] = {"claude_commits": n_all, "one_parent": n_one, "touch_py": n_py, "sampled": len(sample)}
        plan += [(name, c, p) for c, p in sample]
    print(json.dumps(counts, indent=1), file=sys.stderr)
    if a.list:
        return
    t0, results = time.time(), []
    for k, (name, c, p) in enumerate(plan, 1):
        py = [f for f in git(repos[name], "diff", "--name-only", "--no-renames", p, c).split("\n") if f.endswith(".py")]
        try:
            u = replay(repos[name], name, c, p, len(py))
        except Exception as e:
            u = {"repo": name, "commit": c, "parent": p, "py_files": len(py), "scorable": False, "errors": [f"harness: {type(e).__name__}: {str(e)[-300:]}"]}
        results.append(u)
        print(f"[{k}/{len(plan)}] {name} {c[:10]} idx={u.get('index', {}).get('ok')} blocked={u.get('blocked')} prov={len(u.get('findings', []))} adv={len(u.get('advisory', []))} err={u['errors'][:1]}", file=sys.stderr, flush=True)
    entry = {"manifest": {**manifest(repos, counts), "runtime_s": round(time.time() - t0)}, "units": results}
    with open(a.out, "a") as f:
        f.write(json.dumps(entry) + "\n")


def pct(xs, q):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(q * len(xs)))] if xs else float("nan")


def score(a):
    entries = [json.loads(l) for l in open(a.ledger) if l.strip() and json.loads(l)["manifest"].get("bench") == "B6"]
    us = [u for e in entries for u in e["units"]]
    labels = {l["id"]: l for l in map(json.loads, open(a.labels)) if l} if Path(a.labels).is_file() else {}
    advs = sorted((x for u in us for x in u.get("advisory", [])), key=lambda x: x["id"])
    adv_sample = {x["id"] for x in random.Random(SEED).sample(advs, min(40, len(advs)))}
    key = lambda u, f: f"{u['commit'][:12]}:{f['id']}@{f['file']}:{f['line']}"
    todo = [(u["repo"], key(u, f), f) for u in us for f in u.get("findings", []) if key(u, f) not in labels]
    todo += [(u["repo"], x["id"], x) for u in us for x in u.get("advisory", []) if x["id"] in adv_sample and x["id"] not in labels]
    def tally(items):
        c = {k: 0 for k in ("TRUE", "USELESS", "FALSE", "UNSURE")}
        for i in (i for i in items if i in labels):
            c[labels[i]["label"]] += 1
        decided = c["TRUE"] + c["USELESS"] + c["FALSE"]
        return c, (f"{c['TRUE']}/{decided} = {100 * c['TRUE'] / decided:.0f}%" if decided else "n/a")
    print("| repo | units | unscorable | blocked | duplicate | missing | infra-bypass | block precision | advisory precision | Stop p50 / p95 ms |")
    print("| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- | --- | --- |")
    for repo in sorted({u["repo"] for u in us}) + ["all"]:
        ru = [u for u in us if repo == u["repo"] or (repo == "all" and not u["repo"].endswith("-partial"))]
        fs = [(u, f) for u in ru if scorable(u) for f in u.get("findings", [])]
        by = lambda r: sum(f["rule"] == r for _, f in fs)
        stops = [u["stop_ms"] for u in ru if scorable(u)]
        _, bp = tally([key(u, f) for u, f in fs])
        _, ap = tally([x["id"] for u in ru for x in u.get("advisory", []) if x["id"] in adv_sample])
        print(f"| {repo} | {len(ru)} | {sum(not scorable(u) for u in ru)} | {sum(bool(u.get('blocked')) for u in ru if scorable(u))} ({100 * sum(bool(u.get('blocked')) for u in ru if scorable(u)) / max(1, sum(map(scorable, ru))):.0f}%) | {by('duplicate')} | {by('missing')} | {by('infra-bypass')} | {bp} | {ap} | {pct(stops, .5):.0f} / {pct(stops, .95):.0f} |")
    for title, ids in (("blocks (primary)", [key(u, f) for u in us if scorable(u) and not u["repo"].endswith("-partial") for f in u.get("findings", [])]), ("advisory sample", sorted(adv_sample))):
        c, p = tally(ids)
        print(f"\n{title}: n={len(ids)} {c} precision {p}; changed or removed later: {sum(f.get('later_touched') in ('line-changed', 'file-removed') for u in us for f in u.get('findings' if title.startswith('blocks') else 'advisory', []) if (key(u, f) if title.startswith('blocks') else f['id']) in ids)}")
    print(f"\nadvisory total {len(advs)}; unscorable causes:")
    for u in us:
        if not scorable(u):
            print(f"  {u['repo']} {u['commit'][:10]}: {u.get('index', {}).get('error') or (u['errors'] + u.get('hook_errors', []))}")
    print(f"\nlatency: index s p50 {pct([u['index']['s'] for u in us if u.get('index', {}).get('ok')], .5)}; SessionStart ms p50 {pct([u['start_ms'] for u in us if scorable(u)], .5)} p95 {pct([u['start_ms'] for u in us if scorable(u)], .95)}; reconcile ms p50 {pct([u['reconcile_ms'] for u in us if 'reconcile_ms' in u], .5)}, retries {sum(u.get('reconcile_retries', 0) for u in us)}")
    if todo:
        print(f"\nunlabeled ({len(todo)}):")
        for repo, i, f in todo:
            print(f"  {repo} {i} {f['file']}:{f['line']} {f.get('existing', '')}")


# B7 (B7_PREREGISTRATION.md): (group, introducing commit, file, function, group members at the commit's parent).
B7_LABELS = ROOT / "bench/results/b7_labels.jsonl"
POSITIVES = [
    ("env_bool", "c05e4e27", "backend/middleware/security_headers.py", "_env_bool", [("daemon/sandbox_submitter.py", "_env_bool")]),
    ("env_bool", "64ea444b", "backend/middleware/csrf.py", "_env_bool", [("daemon/sandbox_submitter.py", "_env_bool"), ("backend/middleware/security_headers.py", "_env_bool")]),
    ("env_bool", "165849f7", "backend/services/auth_cookies.py", "_env_bool", [("daemon/sandbox_submitter.py", "_env_bool"), ("backend/middleware/security_headers.py", "_env_bool"), ("backend/middleware/csrf.py", "_env_bool")]),
    ("get_secret", "66e09911", "backend/api/cloudflare_webhooks.py", "_get_secret", [("backend/api/darktrace_webhook.py", "_get_secret")]),
    ("verify_signature", "66e09911", "backend/api/cloudflare_webhooks.py", "_verify_signature", [("backend/api/darktrace_webhook.py", "_verify_signature")]),
    ("parse_json", "66e09911", "backend/api/cloudflare_webhooks.py", "_parse_json", [("backend/api/darktrace_webhook.py", "_parse_json")]),
    ("is_dev_mode", "91f40aa6", "backend/services/auth_service.py", "_is_dev_mode", [("backend/api/vstrike.py", "_is_dev_mode")]),
    ("slugify", "bcc689f1", "backend/services/custom_agent_service.py", "slugify", [("scripts/create_workflow.py", "slugify")]),
    ("slugify", "454873c1", "services/custom_workflow_service.py", "_slugify", [("scripts/create_workflow.py", "slugify")]),
    ("slugify", "d82f75e0", "backend/api/llm_providers.py", "_slugify", [("scripts/create_workflow.py", "slugify"), ("backend/services/custom_agent_service.py", "slugify"), ("services/custom_workflow_service.py", "_slugify")]),
]


def b7_population(repo):
    """One-parent commits reachable from origin/HEAD that name a .py file, minus the positives; seeded sample of CAP."""
    rows = []
    for l in git(repo, "log", "--no-renames", "--format=@%H %P", "--name-only", "refs/remotes/origin/HEAD").splitlines():
        if l.startswith("@"):
            rows.append([l[1:].split(), False])
        elif l.endswith(".py"):
            rows[-1][1] = True
    one = [r for r, py in rows if len(r) == 2]
    py = sorted((c, p) for (c, p), touch in ((r, t) for r, t in rows if len(r) == 2) if touch and not any(c.startswith(x[1]) for x in POSITIVES))
    return {"commits": len(rows), "one_parent": len(one), "touch_py_non_positive": len(py)}, sorted(random.Random(SEED).sample(py, CAP))


def b7(a):
    global BIN, BENCH
    BENCH, repo = "B7", Path(a.repo).resolve()
    arms = [s.split(":", 2) for s in a.arm]  # NAME:REF:BINARY
    os.environ["TMPDIR"] = str(Path(a.work) / "tmp")
    Path(os.environ["TMPDIR"]).mkdir(parents=True, exist_ok=True)
    counts, sample = b7_population(repo)
    pos = sorted({git(repo, "rev-parse", c).strip() for _, c, *_ in POSITIVES})
    plan = [("positive", c, git(repo, "rev-parse", c + "^").strip()) for c in pos] + [("sample", c, p) for c, p in sample]
    print(json.dumps(counts), file=sys.stderr)
    if a.list:
        print("\n".join(f"{k} {c[:10]} {p[:10]}" for k, c, p in plan))
        return
    t0, results, cache = time.time(), [], Path(a.work) / "index-cache"
    for k, (kind, c, p) in enumerate(plan, 1):
        py = [f for f in git(repo, "diff", "--name-only", "--no-renames", p, c).split("\n") if f.endswith(".py")]
        first = None
        for arm, _, binary in arms:
            BIN = Path(binary)
            if first is not None and not first.get("index", {}).get("ok"):
                u = {**{x: first[x] for x in ("repo", "commit", "parent", "py_files", "index")}, "scorable": False, "errors": ["arm A index failed"]}
            else:
                try:
                    u = replay(repo, "vigil", c, p, len(py), index_cache=cache, reuse=first is not None)
                except Exception as e:
                    u = {"repo": "vigil", "commit": c, "parent": p, "py_files": len(py), "scorable": False, "errors": [f"harness: {type(e).__name__}: {str(e)[-300:]}"]}
            u["arm"], u["kind"] = arm, kind
            first = first or u
            results.append(u)
            print(f"[{k}/{len(plan)}] {arm} {kind} {c[:10]} idx={u.get('index', {}).get('ok')} blocked={u.get('blocked')} prov={len(u.get('findings', []))} adv={len(u.get('advisory', []))} err={u['errors'][:1]}", file=sys.stderr, flush=True)
    bins = {arm: {"ref": ref, "slop_sha": git(ROOT, "rev-parse", ref).strip(), "binary": binary, "binary_sha256": hashlib.sha256(Path(binary).read_bytes()).hexdigest()} for arm, ref, binary in arms}
    manifest = {"bench": "B7", "arms": bins, "vigil_head": git(repo, "rev-parse", "refs/remotes/origin/HEAD").strip(), "seed": SEED, "cap": CAP, "counts": counts, "platform": platform.platform(),
                "python": platform.python_version(), "index": "arm A indexes each parent; arm B restores that index", "runtime_s": round(time.time() - t0)}
    with open(a.out, "a") as f:
        f.write(json.dumps({"manifest": manifest, "units": results}) + "\n")


def b7_hits(u, file, name, members):
    """(block hit, advisory hit): Stop blocked on a duplicate of `name` in `file` naming a member; an advisory item naming one."""
    dup = [f for f in u.get("findings", []) if f["rule"] == "duplicate" and f["file"] == file and f["id"] in u.get("stop_ids", []) and f["fact"].startswith(f"`{name}`")]
    block = scorable(u) and u.get("blocked") and any(re.search(rf"`[^`]*\b{re.escape(m)}` at {re.escape(mf)}:", f["action"]) for f in dup for mf, m in members)
    adv = any(re.match(rf"`{re.escape(name)}` \({re.escape(file)}:\d+\) (has the same shape as|overlaps existing) `[^`]*\b{re.escape(m)}` \({re.escape(mf)}:", x["text"]) for x in u.get("advisory", []) for mf, m in members)
    return bool(block), adv


def b7_score(a):
    entry = [json.loads(l) for l in open(a.ledger) if l.strip() and json.loads(l)["manifest"].get("bench") == "B7"][-1]
    us, labels = entry["units"], {l["id"]: l for l in map(json.loads, open(a.labels)) if l} if Path(a.labels).is_file() else {}
    arms = list(entry["manifest"]["arms"])
    unit = {(u["arm"], u["commit"][:8]): u for u in us}
    print("| group | file | commit | " + " | ".join(f"arm {x} hit | arm {x} advisory" for x in arms) + " |")
    print("| --- | --- | --- | " + " | ".join("--- | ---" for _ in arms) + " |")
    recall = Counter()
    for g, c, file, name, members in POSITIVES:
        cells = []
        for x in arms:
            hit, adv = b7_hits(unit[(x, c)], file, name, members)
            recall[x] += hit
            cells.append(f"{'yes' if hit else 'no'} | {'yes' if adv else 'no'}")
        print(f"| {g} | {file} | {c} | {' | '.join(cells)} |")
    key = lambda u, f: f"{u['commit'][:12]}:{f['id']}@{f['file']}:{f['line']}"
    todo = set()
    for x in arms:
        for scope in ("all", "sample"):
            ids = [key(u, f) for u in us if u["arm"] == x and scorable(u) and (scope == "all" or u["kind"] == "sample") for f in u.get("findings", []) if f["id"] in u.get("stop_ids", [])]
            c = Counter(labels[i]["label"] for i in ids if i in labels)
            todo |= {i for i in ids if i not in labels}
            print(f"\narm {x} block findings ({scope}): n={len(ids)} {dict(c)} precision {c['TRUE']}/{len(ids)}" + (f" = {100 * c['TRUE'] / len(ids):.0f}%" if ids else ""))
        n_pos = len(POSITIVES)
        print(f"arm {x} recall {recall[x]}/{n_pos} = {100 * recall[x] / n_pos:.0f}%; blocked units {sum(bool(u.get('blocked')) for u in us if u['arm'] == x and scorable(u))}/{sum(scorable(u) for u in us if u['arm'] == x)} scorable; unscorable {[u['commit'][:8] for u in us if u['arm'] == x and not scorable(u)]}")
    if todo:
        print(f"\nunlabeled ({len(todo)}):\n  " + "\n  ".join(sorted(todo)))


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run")
    r.add_argument("--repo", action="append", required=True)
    r.add_argument("--work", default="/tmp/b6")
    r.add_argument("--out", default=str(LEDGER))
    r.add_argument("--list", action="store_true")
    r.add_argument("--allow-partial-index", action="store_true", help="exploratory: continue when only a non-Python indexer failed")
    s = sub.add_parser("score")
    s.add_argument("--ledger", default=str(LEDGER))
    s.add_argument("--labels", default=str(LABELS))
    b = sub.add_parser("b7")
    b.add_argument("--repo", default="/tmp/b7/vigil")
    b.add_argument("--work", default="/tmp/b7")
    b.add_argument("--arm", action="append", default=[], help="NAME:REF:BINARY, arm A first (it indexes)")
    b.add_argument("--out", default=str(LEDGER))
    b.add_argument("--list", action="store_true")
    bs = sub.add_parser("b7-score")
    bs.add_argument("--ledger", default=str(LEDGER))
    bs.add_argument("--labels", default=str(B7_LABELS))
    a = ap.parse_args()
    PARTIAL = getattr(a, "allow_partial_index", False)
    {"run": run, "score": score, "b7": b7, "b7-score": b7_score}[a.cmd](a)
