#!/usr/bin/env python3
"""Agent-output A/B pilot: does compressed context preserve the ability to
*use* existing code (the "additions" case)?

For each target function F (with a known signature), we ask a local model to
write a call to F given two contexts:

  A. full        — the function's whole source file, verbatim
  B. compressed  — the same file after `slop compress` skeletonizes F and its
                   neighbors (signature kept, body dropped)

Grade: does the model's output call F with roughly the right arity? The claim
under test is A ≈ B (compression doesn't degrade usage) at far fewer tokens —
NOT high absolute accuracy (the local model is small). Report both.

Caveats: small N, weak local model (qwen2.5:1.5b) — this is a pilot, not proof.

Usage:  python3 bench/context_ab.py [repo]
"""
import ast
import json
import os
import re
import subprocess
import sys

SLOP = os.path.join(os.path.dirname(__file__), "..", "target", "release", "slop")
MODEL = os.environ.get("OLLAMA_MODEL", "qwen2.5:1.5b")
HOST = os.environ.get("OLLAMA_HOST", "http://localhost:11434")
MAX_TASKS = 10


def top_level_funcs(path):
    """(name, param_count) for module-level defs with >=1 non-self param."""
    try:
        tree = ast.parse(open(path, encoding="utf-8", errors="ignore").read())
    except SyntaxError:
        return []
    out = []
    for node in tree.body:
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            params = [a.arg for a in node.args.args if a.arg != "self"]
            if params:
                out.append((node.name, len(params)))
    return out


def ollama(prompt):
    import urllib.request
    body = json.dumps({"model": MODEL, "prompt": prompt, "stream": False,
                       "options": {"temperature": 0}}).encode()
    req = urllib.request.Request(f"{HOST}/api/generate", body,
                                 {"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=120) as r:
        return json.load(r)["response"]


def ask_call(ctx, fname):
    prompt = (f"Code context:\n```python\n{ctx}\n```\n\n"
              f"Write exactly one line of Python that CALLS the function "
              f"`{fname}` with placeholder arguments matching its parameters. "
              f"Output only the call, no explanation.")
    return ollama(prompt)


def grade(output, fname, true_arity):
    m = re.search(rf"{re.escape(fname)}\s*\((.*?)\)", output, re.S)
    if not m:
        return False
    inner = m.group(1).strip()
    got = 0 if inner == "" else inner.count(",") + 1
    return abs(got - true_arity) <= 1  # lenient: within one arg


def compress(repo, file, edit_file):
    r = subprocess.run([SLOP, "compress", repo, file, "--edit-file", edit_file,
                        "--hops", "0"], capture_output=True, text=True)
    return r.stdout if r.returncode == 0 else None


def main():
    repo = sys.argv[1] if len(sys.argv) > 1 else os.path.expanduser(
        "~/Documents/GitHub/geoguessrbot")
    # gather candidate files (own code, moderate size)
    files = []
    for root, _, fs in os.walk(repo):
        if any(v in root for v in ("/venv", "/site-packages", "/.git")):
            continue
        for f in fs:
            if f.endswith(".py"):
                p = os.path.join(root, f)
                n = sum(1 for _ in open(p, errors="ignore"))
                if 40 <= n <= 400:
                    files.append(os.path.relpath(p, repo))
    files.sort()
    if len(files) < 2:
        print("need >=2 files"); return
    edit_locus = files[0]

    tasks = []
    for rel in files[1:]:
        for name, arity in top_level_funcs(os.path.join(repo, rel)):
            tasks.append((rel, name, arity))
    tasks = tasks[:MAX_TASKS]
    if not tasks:
        print("no callable functions found"); return

    a_pass = b_pass = 0
    a_tok = b_tok = 0
    print(f"model={MODEL}  tasks={len(tasks)}  (grade: calls F within +/-1 arg)\n")
    print(f"  {'function':<32} {'full':>5} {'comp':>5}")
    for rel, name, arity in tasks:
        full = open(os.path.join(repo, rel), encoding="utf-8", errors="ignore").read()
        comp = compress(repo, rel, edit_locus)
        if comp is None:
            continue
        a = grade(ask_call(full, name), name, arity)
        b = grade(ask_call(comp, name), name, arity)
        a_pass += a; b_pass += b
        a_tok += len(full) // 4; b_tok += len(comp) // 4
        print(f"  {name[:31]:<32} {'PASS' if a else 'fail':>5} {'PASS' if b else 'fail':>5}")

    n = len(tasks)
    print(f"\n  full-context usage:       {a_pass}/{n}   (~{a_tok} ctx tokens)")
    print(f"  compressed-context usage: {b_pass}/{n}   (~{b_tok} ctx tokens, "
          f"-{100 - b_tok*100//max(a_tok,1)}%)")
    print("\n  thesis: b_pass ~= a_pass at far fewer tokens => compression "
          "preserves usage/additions ability")


if __name__ == "__main__":
    main()
