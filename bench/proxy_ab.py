#!/usr/bin/env python3
"""Proxy steering A/B against the REAL Anthropic API, via `slop proxy`.

The proxy (crates/slop-proxy) is agent-agnostic: point any Anthropic client at
it and, with `--steer`, it injects the repo's sanctioned-channel policy into the
request's system prompt and logs real token usage. This exercises that end to
end — the same task through two proxies, the only difference being steering:

  A (steer OFF): `slop proxy --log a.jsonl`               -> passthrough
  B (steer ON):  `slop proxy --repo <repo> --steer --log b.jsonl`

Task: implement a function that needs DB access. `<repo>/slop.toml` names the
sanctioned DB channel (vigil: database.service.DatabaseService). If the harness
has value, B reuses that channel where A reaches for a raw DB API.

Grade per generation: sanctioned-channel reuse vs raw DB, plus REAL token counts
from the proxy's own usage log (not an estimate). The first sample per arm is
committed to a git branch (arm-a / arm-b) in a scratch repo so the two outputs
can be diffed.

Usage:  ANTHROPIC_API_KEY=... python3 bench/proxy_ab.py [N] [model] [repo]
  defaults: N=3/arm, model=claude-haiku-4-5-20251001, repo=~/Documents/GitHub/vigil
"""
import json
import os
import re
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SLOP = ROOT / "target/release/slop"
PORT_A, PORT_B = 8788, 8789
UPSTREAM = "https://api.anthropic.com"

TASK = (
    "Add a Python function `count_open_findings() -> int` to this codebase that "
    "returns how many security findings currently have status 'open', reading "
    "from the application's data store. Return ONLY the function code in a "
    "```python block, no prose."
)

SANCTIONED = re.compile(r"\b(DatabaseService|database\.service)\b")
RAW_DB = re.compile(
    r"\b(sqlite3|psycopg2?|pymysql|sqlalchemy|create_engine|cursor|\.execute\(|SELECT\s)\b",
    re.I,
)


def wait_port(port, timeout=10):
    deadline = time.time() + timeout
    while time.time() < deadline:
        with socket.socket() as s:
            s.settimeout(0.3)
            if s.connect_ex(("127.0.0.1", port)) == 0:
                return True
        time.sleep(0.2)
    return False


def start_proxy(port, log, steer_repo=None):
    args = [str(SLOP), "proxy", "--port", str(port), "--upstream", UPSTREAM, "--log", str(log)]
    if steer_repo:
        args += ["--repo", str(steer_repo), "--steer"]
    p = subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if not wait_port(port):
        p.terminate()
        sys.exit(f"proxy on :{port} did not come up")
    return p


def call_via_proxy(port, model, temperature):
    """Send one message-creation request through the proxy; return response text."""
    body = json.dumps({
        "model": model,
        "max_tokens": 500,
        "temperature": temperature,
        "system": "You are contributing code to an existing Python codebase.",
        "messages": [{"role": "user", "content": TASK}],
    }).encode()
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}/v1/messages",
        data=body,
        headers={
            "content-type": "application/json",
            "x-api-key": os.environ["ANTHROPIC_API_KEY"],
            "anthropic-version": "2023-06-01",
        },
    )
    with urllib.request.urlopen(req, timeout=120) as r:
        d = json.load(r)
    parts = [b.get("text", "") for b in d.get("content", []) if b.get("type") == "text"]
    return "".join(parts)


def extract_func(text):
    m = re.search(r"```(?:python)?\n(.*?)```", text, re.S)
    code = m.group(1) if m else text
    i = code.find("def count_open_findings")
    return (code[i:] if i >= 0 else code).rstrip() + "\n"


def classify(code):
    if SANCTIONED.search(code):
        return "sanctioned"
    if RAW_DB.search(code):
        return "raw-db"
    return "other"


def read_log_tokens(path):
    """(mean input, mean output) over the request records in a proxy log."""
    ins, outs = [], []
    if Path(path).exists():
        for line in Path(path).read_text().splitlines():
            try:
                r = json.loads(line)
            except json.JSONDecodeError:
                continue
            if r.get("input_tokens"):
                ins.append(r["input_tokens"])
            if r.get("output_tokens"):
                outs.append(r["output_tokens"])
    mean = lambda xs: round(sum(xs) / len(xs)) if xs else 0
    return mean(ins), mean(outs)


def run_arm(label, port, n, model):
    tally = {"sanctioned": 0, "raw-db": 0, "other": 0}
    first = None
    for i in range(n):
        text = call_via_proxy(port, model, 0.5)
        func = extract_func(text)
        v = classify(func)
        tally[v] += 1
        if first is None:
            first = func
        print(f"  {label} {i+1}/{n}: {v}", flush=True)
    return tally, first or ""


def branch_output(scratch, arm, func):
    """Commit `func` on branch arm-<x> in the scratch git repo, for diffing."""
    subprocess.run(["git", "-C", str(scratch), "checkout", "-q", "-b", f"arm-{arm}", "main"], check=True)
    (scratch / "findings_count.py").write_text(func)
    subprocess.run(["git", "-C", str(scratch), "add", "-A"], check=True)
    subprocess.run(["git", "-C", str(scratch), "-c", "user.email=b@b", "-c", "user.name=bench",
                    "commit", "-qm", f"arm {arm}"], check=True)


def main():
    n = int(sys.argv[1]) if len(sys.argv) > 1 else 3
    model = sys.argv[2] if len(sys.argv) > 2 else "claude-haiku-4-5-20251001"
    repo = Path(sys.argv[3]) if len(sys.argv) > 3 else Path.home() / "Documents/GitHub/vigil"
    if not os.environ.get("ANTHROPIC_API_KEY"):
        sys.exit("ANTHROPIC_API_KEY not set")
    if not (repo / "slop.toml").exists():
        sys.exit(f"{repo}/slop.toml missing — steering has nothing to inject")

    work = Path(tempfile.mkdtemp(prefix="slop-proxyab-"))
    log_a, log_b = work / "a.jsonl", work / "b.jsonl"
    scratch = work / "scratch"
    scratch.mkdir()
    subprocess.run(["git", "-C", str(scratch), "init", "-q", "-b", "main"], check=True)
    (scratch / "findings_count.py").write_text("# task: count_open_findings\n")
    subprocess.run(["git", "-C", str(scratch), "add", "-A"], check=True)
    subprocess.run(["git", "-C", str(scratch), "-c", "user.email=b@b", "-c", "user.name=bench",
                    "commit", "-qm", "seed"], check=True)

    pa = start_proxy(PORT_A, log_a)
    pb = start_proxy(PORT_B, log_b, steer_repo=repo)
    try:
        print(f"model={model}  N={n}/arm  repo={repo}")
        print("Arm A (proxy, steer OFF):")
        a_tally, a_first = run_arm("A", PORT_A, n, model)
        print("Arm B (proxy, steer ON — DB channel injected):")
        b_tally, b_first = run_arm("B", PORT_B, n, model)
    finally:
        pa.terminate()
        pb.terminate()

    a_in, a_out = read_log_tokens(log_a)
    b_in, b_out = read_log_tokens(log_b)
    branch_output(scratch, "a", a_first)
    branch_output(scratch, "b", b_first)

    def pct(t, k):
        return 100 * t[k] / n

    print("\n=== RESULTS (tokens from the proxy's own usage log) ===")
    print(f"{'':<28}{'A (off)':>12}{'B (on)':>12}")
    print(f"{'input tokens (mean)':<28}{a_in:>12}{b_in:>12}")
    print(f"{'output tokens (mean)':<28}{a_out:>12}{b_out:>12}")
    print(f"{'sanctioned DB channel (%)':<28}{pct(a_tally,'sanctioned'):>11.0f}{pct(b_tally,'sanctioned'):>12.0f}")
    print(f"{'raw DB (%)':<28}{pct(a_tally,'raw-db'):>11.0f}{pct(b_tally,'raw-db'):>12.0f}")
    print(f"\nsteering token cost: +{b_in - a_in} input tokens/request")
    print(f"adherence: {pct(a_tally,'sanctioned'):.0f}% -> {pct(b_tally,'sanctioned'):.0f}% sanctioned")

    print("\n=== OUTPUT DIFF (arm-a vs arm-b) ===")
    diff = subprocess.run(["git", "-C", str(scratch), "diff", "--no-color", "arm-a", "arm-b"],
                          capture_output=True, text=True).stdout
    print(diff or "(identical)")
    print(f"scratch repo (branches arm-a/arm-b): {scratch}\nlogs: {log_a} {log_b}\n"
          f"clean up: rm -rf {work}")


if __name__ == "__main__":
    main()
