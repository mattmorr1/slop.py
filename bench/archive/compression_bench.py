#!/usr/bin/env python3
"""Token-efficiency A/B for slop's zoned compression.

For each dogfood repo, take an edit locus (one file) and measure the context
cost of *reading another file* two ways:

  A. naive   — the whole file at full fidelity (what an agent reads today)
  B. slop    — the file zoned-compressed against the edit locus (read hook)

Reports the token reduction (tokens ~= chars / 4). This is the deterministic
half of the harness thesis: skeletonize what the edit isn't near.

Usage:  python3 bench/compression_bench.py [repo ...]
Default repos: ~/Documents/GitHub/{stress-analysis,geoguessrbot,vigil}
"""
import json
import os
import subprocess
import sys

SLOP = os.path.join(os.path.dirname(__file__), "..", "target", "release", "slop")
if not os.path.exists(SLOP):
    SLOP = os.path.join(os.path.dirname(__file__), "..", "target", "debug", "slop")

DEFAULT_REPOS = [
    os.path.expanduser(f"~/Documents/GitHub/{r}")
    for r in ("stress-analysis", "geoguessrbot", "vigil")
]
HOPS = 1
TOP_N = 6          # largest files per repo to measure
MIN_LINES = 40     # skip trivially small files


VENDOR = ("/.git", "/node_modules", "/venv", "/.venv", "/site-packages",
          "/.tox", "/build", "/dist", "/__pycache__", "/.mypy_cache")


def py_files(repo):
    out = []
    for root, _, files in os.walk(repo):
        if any(v in root for v in VENDOR):
            continue
        for f in files:
            if f.endswith(".py"):
                p = os.path.join(root, f)
                try:
                    n = sum(1 for _ in open(p, encoding="utf-8", errors="ignore"))
                except OSError:
                    continue
                if n >= MIN_LINES:
                    out.append((n, os.path.relpath(p, repo)))
    out.sort(reverse=True)
    return [f for _, f in out]


def compress(repo, file, edit_file):
    """Return (original_chars, compressed_chars) for reading `file` while
    editing `edit_file`."""
    r = subprocess.run(
        [SLOP, "compress", repo, file, "--edit-file", edit_file,
         "--hops", str(HOPS), "--stats"],
        capture_output=True, text=True,
    )
    if r.returncode != 0:
        return None
    # stats line: "compress: X/Y functions skeletonized, A -> B chars (-P%)"
    for line in r.stderr.splitlines():
        if line.startswith("compress:"):
            seg = line.split(",")[1]  # " A -> B chars (-P%)"
            a, b = seg.replace("chars", "").split("->")
            skeleton = line.split(",")[0].split()[1]  # "X/Y"
            return int(a.strip()), int(b.strip().split()[0]), skeleton
    return None


def bench_repo(repo):
    files = py_files(repo)
    if len(files) < 2:
        print(f"  (skip: <2 sizeable .py files)")
        return None
    # Edit locus: the *smallest* of the top files (a plausible focused edit);
    # measure reads of the rest.
    locus = files[min(TOP_N - 1, len(files) - 1)]
    targets = [f for f in files[:TOP_N] if f != locus]
    tot_a = tot_b = 0
    rows = []
    for t in targets:
        res = compress(repo, t, locus)
        if not res:
            continue
        a, b, sk = res
        tot_a += a
        tot_b += b
        rows.append((t, a, b, sk))
    if not rows:
        return None
    print(f"  edit locus: {locus}")
    print(f"  {'file':<45} {'orig':>7} {'slop':>7} {'save':>6} skel")
    for t, a, b, sk in rows:
        pct = 100 - b * 100 // max(a, 1)
        print(f"  {t[:44]:<45} {a:>7} {b:>7} {pct:>5}% {sk}")
    pct = 100 - tot_b * 100 // max(tot_a, 1)
    print(f"  {'TOTAL':<45} {tot_a:>7} {tot_b:>7} {pct:>5}%   "
          f"(~{tot_a//4} -> ~{tot_b//4} tokens)")
    return tot_a, tot_b


def main():
    repos = sys.argv[1:] or DEFAULT_REPOS
    g_a = g_b = 0
    for repo in repos:
        name = os.path.basename(repo.rstrip("/"))
        if not os.path.exists(os.path.join(repo, "index.scip")):
            print(f"\n== {name}: no index.scip, skipping ==")
            continue
        print(f"\n== {name} ==")
        res = bench_repo(repo)
        if res:
            g_a += res[0]
            g_b += res[1]
    if g_a:
        pct = 100 - g_b * 100 // g_a
        print(f"\n== OVERALL: {g_a} -> {g_b} chars (-{pct}%), "
              f"~{g_a//4} -> ~{g_b//4} tokens ==")


if __name__ == "__main__":
    main()
