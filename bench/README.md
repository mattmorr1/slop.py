# Benchmarks

Machine-readable runs are appended to `bench/results/*.jsonl`, each with a
manifest (commit, clean/dirty, seed, corpus digest, platform). Every run here
is labelled *exploratory*: one corpus family, no pre-registered hypothesis.

## `equiv_bench.py` — B3, the equivalence ladder

Can a hash deny a write? Only if it never calls two different functions equal.
Ground truth is by construction: 400 functions sampled (seed 20260923) from
7,078 unique Python functions in stress-analysis, geoguessrbot and vigil, each
paired with mutants. **P** mutants are provably equivalent for every Python value
(consistent rename, ternary ↔ branches, negate-and-swap, dead code after an exit,
temp inline, implicit `return None`, int folding, `pass`, 2–3 combined). **B real**
mutants change behaviour (constant, callee, keyword name, comparison, negated
test, swapped calls, default, `async`, annotation). **B trap** mutants apply laws true only for some
types (`x += y`, `a + b → b + a`, De Morgan, comparison flips).

```
cargo build --release -p slop-cli --features egraph
python3 bench/equiv_bench.py --limit 400
```

### Result (commit c19cf71, 3,759 pairs, 2026-09-23)

| | exact | same shape | α v1 | α v2 | **E-sound** | egglog sound | E-graded | egglog graded |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| recall on P (n=1,835) | 0.3% | 21.7% | 16.2% | 21.6% | **99.1%** | 99.0% | 99.1% | 99.0% |
| false equiv., B real (n=1,813) | 28.4% | 73.4% | 36.9% | 0.0% | **0.0%** | 0.0% | 0.0% | 0.0% |
| false equiv., B trap (n=111) | 0.0% | 4.5% | 0.0% | 0.0% | **0.0%** | 0.0% | 93.7% | 91.0% |

Latency per pair (both sides): normalizer p50 117 µs, p95 364 µs; egglog sound
p50 2.5 ms, graded p50 4.6 ms (release, M-series, one core).

What it says:

- **E-sound finds 4.6× the duplicates α does with zero false equivalences**
  (rule of three: true rate < 0.17% at 95% on this distribution).
- **Body-only hashes are unsafe to deny on**: α v1 calls 36.9% of behaviour
  changes equal: 73% of keyword-name changes, and 100% of changed defaults,
  `def` → `async def` and annotation changes (the last two added in c19cf71
  after a real-code review found E-sound ignored them too; see f7150bc).
- **"Same shape" is not evidence of duplication**: 73.4% false equivalence.
- **The graded tier is advisory by design**: it equates 93.7% of the traps,
  which is what "holds only for well-behaved types" costs.
- **egglog as oracle, not engine** (ADR 0004): it found a graded confluence bug
  in run 1 (6358902: 1810 vs 1814) and, once fixed, never proved a pair the
  normalizer missed; the normalizer is 25–45× faster.

Residual misses: integer literals inside f-strings (opaque), and a few 3-way
combinations. Caveats: mutants are synthetic rewrites of real code, not
naturally occurring duplicates; the corpus is three AI-assisted repos.

## `compression_bench.py` — token-efficiency A/B

Quantifies the deterministic half of the harness thesis (D11): zoned
graph-distance compression keeps the code an edit is *near* at full fidelity
and skeletonizes the rest. For each repo it picks an edit locus (one file) and
measures the cost of reading other large files two ways — naive (whole file)
vs slop (zoned-compressed) — reporting the reduction (tokens ≈ chars / 4).

```
cargo build --release
python3 bench/compression_bench.py [repo ...]
```

Defaults to `~/Documents/GitHub/{stress-analysis,geoguessrbot,vigil}` (needs a
`<repo>/index.scip`). Vendored dirs (`venv`, `site-packages`, …) are excluded —
slop doesn't index them, so they'd skeletonize 0% and skew the numbers.

### Latest results (2026-07-05, hops=1, top-6 files/repo)

| Repo | orig chars | slop chars | reduction |
| --- | ---: | ---: | ---: |
| stress-analysis | 54,109 | ~18.8k | **−66%** |
| geoguessrbot | 57,003 | ~18.4k | **−68%** |
| vigil (app code) | 533,611 | ~215k | **−60%** |
| **overall** | **644,723** | **257,695** | **−61%** (~161k → ~64k tokens) |

Files near the edit locus keep more of their body by design (e.g. vigil
`database/models.py` −32% — it's imported/called by the edited code), which is
the point: the compression is *relevance-aware*, not blind truncation.

**Integrity:** the compressed view is a read-path artifact — the source files
on disk are never touched. And the view stays *valid Python*: all 23 compressed
files across the three repos parse with `ast.parse` (skeletons preserve their
original indentation, decorators included).

## `context_ab.py` — agent-output A/B (usage/additions)

Tests whether compressed context preserves the ability to *use* existing code
(the additions case). For each target function it asks a local model to write a
call, given full-file context vs `slop compress`-skeletonized context, and
grades arity. The claim under test is **A ≈ B at fewer tokens**, not high
absolute accuracy.

```
cargo build --release
python3 bench/context_ab.py [repo]     # OLLAMA_MODEL overridable
```

### Pilot (2026-07-06, geoguessrbot, qwen2.5:1.5b, N=10)

| context | usage correct | ctx tokens |
| --- | ---: | ---: |
| full file | 10/10 | ~26,392 |
| compressed | 10/10 | ~14,652 (**−45%**) |

Compression cost nothing on usage while roughly halving tokens — consistent
with the mechanism (skeletons keep the signature an addition needs; only the
body, which a *caller* doesn't need, is dropped).

**Caveats — this is a pilot, not proof.** N=10, a small model, lenient grading,
one repo, and a ceiling effect (both perfect) that a harder task set would
break. It tests *using* skeletonized code, **not modifying** it — modification
needs the body, which is why edit targets are kept full-fidelity (see
`docs/harness.md` edit-invertibility).

## `steering_ab.py` — the steering value A/B (introduced-slop)

Tests the *other* half of the thesis, the one the compression benches don't
touch: does injecting the repo's sanctioned-channel policy (what `slop hook
user-prompt-submit` does) make a model **reuse infrastructure instead of
introducing infra-bypass slop**? Designed around the last pilot's ceiling-effect
failure: a *new-file* task with neutral context, so the sanctioned channel is
invisible unless the harness surfaces it.

- **A (off):** bare task.
- **B (on):** real harness steering + the channel interface `query_subgraph` /
  `get_context_envelope` would surface (inlined, since a one-shot `generate`
  can't call tools). Without the harness the agent has no way to know the
  channel exists — that asymmetry *is* the harness's value, not extra hinting.
- **Metric:** does the new code use the sanctioned `HttpClient` (adherent) or
  reach for raw net — `urllib`/`requests`/`httpx`/… (slop)?

```
cargo build --release          # steering string comes from the real binary
python3 bench/steering_ab.py [N] [model]
```

### Result (2026-07-06, toy_repo fixture, qwen2.5:1.5b, N=20/condition)

| | A (off) | B (on) |
| --- | ---: | ---: |
| sanctioned-channel adherence | 0% | **30%** |
| raw-net slop | 100% | 70% |
| mean output tokens | 62 | 108 |

**0/20 → 6/20** sanctioned, one-sided Fisher exact **p = 0.010** — a real
effect, not the last pilot's null. The baseline *never* reuses the channel (it
can't; it doesn't know it exists); the harness makes reuse happen. This is the
first statistically-significant evidence for the steering claim.

## `function_ab.py` — implement-a-function A/B (local ollama)

Extends the additions test to *implementing a function body* with a local model,
grading parse/adherence and materializing each arm on a git branch/worktree to
diff. **Known confound:** arm A is handed the full channel file while arm B gets
the compressed skeleton + steering, so it entangles "more context" with
"steering." Kept as a local (no-API) harness; for a *clean* steering result use
`proxy_ab.py`. Instructive finding: given the full file, even an 8B reuses the
channel unprompted — the harness's value is surfacing it *without* the dump.

## `proxy_ab.py` — steering A/B through `slop proxy` (real Anthropic API)

The cleanest steering test: the same message-creation request sent through two
proxies whose *only* difference is `--steer`, so there's no context confound.
Tokens come from the proxy's own usage log (real, not estimated). Needs
`ANTHROPIC_API_KEY` and a `<repo>/slop.toml` naming the sanctioned channel.

```
cargo build --release
ANTHROPIC_API_KEY=... python3 bench/proxy_ab.py [N] [model] [repo]
```

### Result (2026-07-06, vigil DB channel, claude-haiku-4-5, N=3/arm)

| | A (steer off) | B (steer on) |
| --- | ---: | ---: |
| sanctioned DB channel | 0% | **100%** |
| input tokens (mean) | 74 | 110 (+36) |
| output tokens (mean) | 98 | 102 |

Without steering, Haiku **hallucinated** infra vigil doesn't have
(`django.apps.get_model('security','Finding')` — vigil isn't Django); with 36
tokens of steering it reused the real `database.service.DatabaseService`. A
capable model absent context doesn't reach for raw SQL — it invents a
plausible-but-wrong framework, which is exactly the slop the harness prevents.

**Caveats.** One task, N=3, a syntactic adherence proxy. Steered Claude reused
the right *class* but guessed the method (`query_findings` vs the real
`get_findings`) — surfacing the signature is the MCP `query_subgraph` tool's job,
which a one-shot proxy request can't call. vigil has no auto-inferable *net*
channel (its ~20 `tools/` integrations each use raw httpx — infra-bypass at
scale); the DB channel policy was hand-authored for this run.

## Steering pilot caveats (steering_ab.py)

One task, one tiny model, N=20. A 1.5B still reaches for raw net
70% of the time even handed the interface — so the harness *shifts* behaviour
but a weak model caps the ceiling low; the effect should grow with model
capability. Adherence is scored by a syntactic proxy (which import/type the new
code uses), not a full `slop check` reindex of the output. A capable model and
more tasks would sharpen the estimate.
