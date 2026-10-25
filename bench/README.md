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

## Archive

The scripts in `bench/archive/` measured surfaces removed in P0 (the context
envelope, read compression, the proxy) or one-off spikes. They are kept for
provenance and do not run against the current binary; their result ledgers
stay in `bench/results/`. Headline numbers:

- **B4, context selection against co-change history** (`context_bench.py`).
  1,326 tasks over vigil, requests, flask and httpx, recall of co-changed
  functions, out of sample. At 4k tokens the calibrated selector reached 66.2%
  against BM25's 59.3% and file proximity's 57.0% (7dc5255). After hub pruning
  (ef6e218) the adaptive default reached 65.1% at 4k and 76.0% at 8k, +6.3
  points over BM25 at 8k [+2.4, +10.0], and Sentry context p95 fell from
  263 ms to 9 ms.
- **R8 run 1, does context change helper reuse?** (`reuse_bench.py`,
  pre-registered in 7e52daa, result eb7356e). 100 gutted functions,
  qwen2.5-coder:7b and llama3.1:8b. **Negative**: the envelope reached 30.2%
  reuse against BM25's 28.6% (H1 not supported) and the target's own file's
  36.4% (H2 reversed, -6.3 points [-12.0, -0.8]). B4's recall win did not
  carry over: co-change recall is not the same as showing a model how to call
  things.
- **R8 run 2, file plus envelope against file plus BM25** (pre-registered in
  6c08089, result c45c2f1). **Negative**: file+slop 37.5%, file+bm25 40.0%,
  file alone 39.0%; no hypothesis that favoured the envelope was supported. At
  7-8B the target's own file carries almost all the usable signal.
- **Steering pilots.** `steering_ab.py` (b0b8ef2, toy_repo, qwen2.5:1.5b,
  N=20 per arm): injecting the sanctioned-channel policy moved adherence from
  0/20 to 6/20, one-sided Fisher p = 0.010. The proxy A/B (66f311f, vigil DB
  channel, claude-haiku-4-5, N=3 per arm) went from 0% to 100% sanctioned for
  36 extra input tokens; unsteered, the model invented a Django API vigil does
  not have. Both are pilots: one task each, small N, syntactic scoring.
- **Read compression** (`compression_bench.py`; `context_ab.py` pilot f82ec24):
  zoned compression cut read tokens by 61% across three repos, and a pilot at
  N=10 showed no loss on writing calls against skeletonized context.
