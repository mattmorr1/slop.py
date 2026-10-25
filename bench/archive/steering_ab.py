#!/usr/bin/env python3
"""Value A/B for the harness's *steering* (not compression, already measured).

Thesis: injecting the repo's sanctioned-channel policy on the prompt (what
`slop hook user-prompt-submit` does) makes even a small local model reuse the
codebase's infrastructure instead of reaching for raw I/O — i.e. produce less
infra-bypass slop.

Design that avoids the last pilot's ceiling effect: a *new-file* task with
neutral context, so the sanctioned channel is invisible unless steering injects
it. If the harness has value, condition B (steering on) uses the sanctioned
`HttpClient` where condition A (off) writes `urllib`/`requests`.

Metric per generation: does the new code use the sanctioned channel, reach for
raw net (slop), or neither? Plus output-token count. Run N per condition at
temperature>0 to get a rate, not a single deterministic sample.

Usage: python bench/steering_ab.py [N] [model]
  (requires `ollama serve`; defaults N=20, model=qwen2.5:1.5b)
"""
import json
import re
import subprocess
import sys
import urllib.request
from pathlib import Path

FIXTURE = Path(__file__).resolve().parent.parent / "tests/fixtures/toy_repo"
SLOP = Path(__file__).resolve().parent.parent / "target/release/slop"
OLLAMA = "http://localhost:11434/api/generate"

TASK = (
    "You are adding code to an existing Python codebase. Write a function "
    "`fetch_path(path: str) -> bytes` that performs an HTTP GET for `path` "
    "relative to the base URL 'https://api.example.com' and returns the raw "
    "response body. Return only the Python code, no prose."
)

# What the harness *surfaces on demand* via `query_subgraph` /
# `get_context_envelope` once the policy points at the channel. A one-shot
# `generate` can't call those tools, so condition B inlines what they'd return
# — the sanctioned channel's interface. This is the harness's read-path value,
# not extra hand-holding: without the harness the agent has no way to know the
# channel exists (condition A).
CHANNEL_SPEC = (
    "The sanctioned net channel's interface (from `query_subgraph core.http_client.HttpClient`):\n"
    "    class HttpClient:  # core.http_client — reuse this for HTTP\n"
    "        def __init__(self, base_url: str, retries: int = 3): ...\n"
    "        def get(self, path: str) -> bytes: ...  # GET path relative to base_url\n"
)

RAW_NET = re.compile(
    r"\b(urllib|requests|httpx|http\.client|socket|urlopen|aiohttp)\b"
)
SANCTIONED = re.compile(r"\b(HttpClient|http_client)\b")


def real_steering() -> str:
    """The exact steering string the harness injects for this repo."""
    out = subprocess.run(
        [str(SLOP), "hook", "user-prompt-submit"],
        input=json.dumps({"cwd": str(FIXTURE)}),
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    return json.loads(out)["hookSpecificOutput"]["additionalContext"]


def generate(prompt: str, model: str, temperature: float = 0.8) -> tuple[str, int]:
    body = json.dumps(
        {
            "model": model,
            "prompt": prompt,
            "stream": False,
            "options": {"temperature": temperature},
        }
    ).encode()
    req = urllib.request.Request(OLLAMA, data=body, headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=120) as resp:
        d = json.load(resp)
    return d["response"], int(d.get("eval_count", 0))


def classify(code: str) -> str:
    """sanctioned | slop | other — 'slop' = introduced raw net (a bypass)."""
    raw = RAW_NET.search(code)
    chan = SANCTIONED.search(code)
    if raw:
        return "slop"  # reached for raw I/O regardless of anything else
    if chan:
        return "sanctioned"
    return "other"


def run(label: str, prompt: str, n: int, model: str) -> dict:
    tally = {"sanctioned": 0, "slop": 0, "other": 0}
    tokens = []
    for i in range(n):
        code, tok = generate(prompt, model)
        verdict = classify(code)
        tally[verdict] += 1
        tokens.append(tok)
        print(f"  {label} {i+1}/{n}: {verdict} ({tok} tok)", flush=True)
    tally["mean_tokens"] = round(sum(tokens) / max(len(tokens), 1))
    return tally


def main() -> None:
    n = int(sys.argv[1]) if len(sys.argv) > 1 else 20
    model = sys.argv[2] if len(sys.argv) > 2 else "qwen2.5:1.5b"
    steering = real_steering()
    print(f"model={model}  N={n} per condition")
    print(f"steering injected:\n{steering}\n")

    print("Condition A (harness OFF — bare task):")
    a = run("A", TASK, n, model)
    print("Condition B (harness ON — steering + surfaced channel + task):")
    b_prompt = steering + "\n\n" + CHANNEL_SPEC + "\n" + TASK
    b = run("B", b_prompt, n, model)

    def pct(t, k):
        return 100 * t[k] / n
    print("\n=== RESULTS ===")
    print(f"{'':<26}{'A (off)':>12}{'B (on)':>12}")
    for k in ("sanctioned", "slop", "other"):
        print(f"{k+' (%)':<26}{pct(a,k):>11.0f}{pct(b,k):>12.0f}")
    print(f"{'mean output tokens':<26}{a['mean_tokens']:>12}{b['mean_tokens']:>12}")
    print(
        f"\nsanctioned-channel adherence: {pct(a,'sanctioned'):.0f}% -> {pct(b,'sanctioned'):.0f}% "
        f"({b['sanctioned']-a['sanctioned']:+d}/{n}); "
        f"raw-net slop: {pct(a,'slop'):.0f}% -> {pct(b,'slop'):.0f}%"
    )


if __name__ == "__main__":
    main()
