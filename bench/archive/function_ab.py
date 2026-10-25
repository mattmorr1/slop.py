#!/usr/bin/env python3
"""Function-implementation A/B: does the slop harness make a local model (a) use
fewer context tokens and (b) produce a better function, vs. no harness?

Unlike context_ab.py (writes a *call*) and steering_ab.py (writes a *new file*
with no in-repo grading), this runs the full loop for *implementing a function
body* and grades the result two ways — token cost and outcome quality — then
materializes each arm's output on its own git branch/worktree so the two can be
diffed.

  A (harness OFF): raw dump of the channel's source file + the stub + "implement".
  B (harness ON):  real `slop hook user-prompt-submit` steering + the *compressed*
                   channel skeleton (`slop compress`) + the stub.

Task: implement `send_daily_report(url, body) -> int`, which needs an HTTP POST.
The repo routes net I/O through the sanctioned `core.http_client.HttpClient`, so
the *good* implementation reuses it; reaching for raw `urllib`/`requests` is slop.

Grade per generation: parses? implements the signature? uses the sanctioned
channel vs raw net? Plus real token counts from ollama (prompt_eval_count /
eval_count, not a char/4 estimate).

Usage:  python3 bench/function_ab.py [N] [model]
  (needs `ollama serve`; defaults N=3 per arm, model=qwen2.5:1.5b)
"""
import ast
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SLOP = ROOT / "target/release/slop"
SEED = ROOT / "tests/fixtures/toy_repo"
OLLAMA = os.environ.get("OLLAMA_HOST", "http://localhost:11434") + "/api/generate"

STUB_FILE = "services/report.py"
STUB = (
    "def send_daily_report(url: str, body: str) -> int:\n"
    '    """POST the daily report `body` to `url`. Return the HTTP status code."""\n'
    "    raise NotImplementedError\n"
)
TASK = (
    "Implement the following function. Return ONLY the complete function code "
    "(a ```python block), no prose:\n\n```python\n" + STUB + "```"
)

RAW_NET = re.compile(r"\b(urllib|requests|httpx|http\.client|socket|urlopen|aiohttp)\b")
SANCTIONED = re.compile(r"\b(HttpClient|http_client)\b")


def sh(*args, **kw):
    return subprocess.run(args, capture_output=True, text=True, **kw)


def generate(prompt: str, model: str, temperature: float):
    """Return (response_text, input_tokens, output_tokens) from ollama."""
    body = json.dumps(
        {"model": model, "prompt": prompt, "stream": False,
         "options": {"temperature": temperature}}
    ).encode()
    req = urllib.request.Request(OLLAMA, data=body, headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=300) as resp:
        d = json.load(resp)
    return d["response"], int(d.get("prompt_eval_count", 0)), int(d.get("eval_count", 0))


def real_steering(repo: Path) -> str:
    out = sh(str(SLOP), "hook", "user-prompt-submit", input=json.dumps({"cwd": str(repo)})).stdout
    try:
        return json.loads(out)["hookSpecificOutput"]["additionalContext"]
    except Exception:
        return ""


def compress(repo: Path, file: str) -> str:
    r = sh(str(SLOP), "compress", str(repo), file, "--hops", "0")
    return r.stdout if r.returncode == 0 else ""


def extract_func(text: str) -> str:
    """Pull the function source out of a model reply (strip fences, keep from the
    first `def send_daily_report`)."""
    m = re.search(r"```(?:python)?\n(.*?)```", text, re.S)
    code = m.group(1) if m else text
    i = code.find("def send_daily_report")
    return (code[i:] if i >= 0 else code).rstrip() + "\n"


def grade(func_src: str) -> dict:
    parses = True
    try:
        ast.parse(func_src)
    except SyntaxError:
        parses = False
    has_sig = "def send_daily_report" in func_src
    raw, chan = RAW_NET.search(func_src), SANCTIONED.search(func_src)
    adherence = "slop" if raw else ("sanctioned" if chan else "other")
    return {"parses": parses, "has_sig": has_sig, "adherence": adherence}


def seed_repo(dst: Path):
    """Temp git repo = toy_repo + the stub file to implement."""
    shutil.copytree(SEED, dst, dirs_exist_ok=True)
    (dst / STUB_FILE).write_text(STUB)
    sh("git", "init", "-q", cwd=dst)
    sh("git", "add", "-A", cwd=dst)
    sh("git", "-c", "user.email=b@b", "-c", "user.name=bench", "commit", "-qm", "seed", cwd=dst)


def worktree_with_output(repo: Path, arm: str, func_src: str):
    """Create branch `arm-<x>` in its own worktree, write the generated function
    into the stub file, and commit. Returns the worktree path."""
    wt = repo.parent / f"wt-{arm}"
    sh("git", "worktree", "add", "-q", "-b", f"arm-{arm}", str(wt), "HEAD", cwd=repo)
    (wt / STUB_FILE).write_text(func_src)
    sh("git", "add", "-A", cwd=wt)
    sh("git", "-c", "user.email=b@b", "-c", "user.name=bench", "commit", "-qm", f"arm {arm}", cwd=wt)
    return wt


def run_arm(label: str, prompt: str, n: int, model: str, temp: float):
    tally = {"sanctioned": 0, "slop": 0, "other": 0}
    parse_ok = sig_ok = 0
    in_toks, out_toks, samples = [], [], []
    for i in range(n):
        text, itok, otok = generate(prompt, model, temp)
        func = extract_func(text)
        g = grade(func)
        tally[g["adherence"]] += 1
        parse_ok += g["parses"]
        sig_ok += g["has_sig"]
        in_toks.append(itok)
        out_toks.append(otok)
        samples.append(func)
        print(f"  {label} {i+1}/{n}: {g['adherence']:<10} "
              f"parse={'y' if g['parses'] else 'n'} sig={'y' if g['has_sig'] else 'n'} "
              f"in={itok} out={otok}", flush=True)
    return {
        "tally": tally, "parse_ok": parse_ok, "sig_ok": sig_ok,
        "in_mean": round(sum(in_toks) / max(len(in_toks), 1)),
        "out_mean": round(sum(out_toks) / max(len(out_toks), 1)),
        "sample": samples[0] if samples else "",
    }


def main():
    n = int(sys.argv[1]) if len(sys.argv) > 1 else 3
    model = sys.argv[2] if len(sys.argv) > 2 else "qwen2.5:1.5b"
    if not SLOP.exists():
        sys.exit("build first: cargo build --release")

    work = Path(tempfile.mkdtemp(prefix="slop-fnab-"))
    repo = work / "repo"
    seed_repo(repo)

    channel_full = (repo / "core/http_client.py").read_text()
    channel_comp = compress(repo, "core/http_client.py") or channel_full
    steering = real_steering(repo)

    prompt_a = (f"Here is a file from the codebase:\n```python\n{channel_full}\n```\n\n{TASK}")
    prompt_b = (f"{steering}\n\nRelevant interface (compressed):\n"
                f"```python\n{channel_comp}\n```\n\n{TASK}")

    print(f"model={model}  N={n}/arm  repo={repo}")
    print(f"channel file: {len(channel_full)} chars full -> {len(channel_comp)} chars compressed\n")

    print("Arm A (harness OFF — raw file dump):")
    a = run_arm("A", prompt_a, n, model, 0.4)
    print("Arm B (harness ON — steering + compressed channel):")
    b = run_arm("B", prompt_b, n, model, 0.4)

    wt_a = worktree_with_output(repo, "a", a["sample"])
    wt_b = worktree_with_output(repo, "b", b["sample"])

    def pct(t, k):
        return 100 * t["tally"][k] / n

    print("\n=== RESULTS ===")
    print(f"{'':<28}{'A (off)':>12}{'B (on)':>12}")
    print(f"{'input (ctx) tokens':<28}{a['in_mean']:>12}{b['in_mean']:>12}")
    print(f"{'output tokens':<28}{a['out_mean']:>12}{b['out_mean']:>12}")
    print(f"{'total tokens':<28}{a['in_mean']+a['out_mean']:>12}{b['in_mean']+b['out_mean']:>12}")
    print(f"{'parses':<28}{a['parse_ok']:>10}/{n}{b['parse_ok']:>10}/{n}")
    print(f"{'sanctioned-channel (%)':<28}{pct(a,'sanctioned'):>11.0f}{pct(b,'sanctioned'):>12.0f}")
    print(f"{'raw-net slop (%)':<28}{pct(a,'slop'):>11.0f}{pct(b,'slop'):>12.0f}")

    itot_a, itot_b = a["in_mean"], b["in_mean"]
    delta = 100 - (itot_b * 100 // max(itot_a, 1))
    print(f"\ninput-token change A->B: {delta:+d}%  "
          f"(compression saves, steering adds — net shown)")
    print(f"adherence A->B: {pct(a,'sanctioned'):.0f}% -> {pct(b,'sanctioned'):.0f}% sanctioned")

    print("\n=== OUTPUT DIFF (arm-a vs arm-b) ===")
    diff = sh("git", "-C", str(repo), "diff", "--no-color", "arm-a", "arm-b", "--", STUB_FILE).stdout
    print(diff or "(identical output)")
    print(f"worktrees left for inspection:\n  A: {wt_a}\n  B: {wt_b}\n"
          f"clean up: rm -rf {work}")


if __name__ == "__main__":
    main()
