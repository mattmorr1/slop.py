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

## `b5_bench.py` — B5, bind recall (gate G1)

The B5 harness lives in `bench/archive/` with the R8 harness it imports, and
runs from a checkout of 278c7fb (its envelope arm needs the pre-P0 binary).

Pre-registered in `bench/archive/B5_PREREGISTRATION.md` (8c7d9f2) before any
generation; two pre-scoring binder/arm changes and the vigil pinning are
listed there. Question:
does binding a context-free sketch (`b5_bind.py`, binder v0: name resolution,
MCP `assess_write` callee overlap and E-sound, per-call BM25, merged by
reciprocal rank) surface the target's real callees better than the envelope
built for the gutted target? R8's 100 targets; flask `d73fa1cd` and httpx
`b5addb64` reproduce R8's archived tasks exactly; vigil is pinned at
`f61607df` (below). Sketches: prompts `none`
(R8's) and `sketch`, llama3.1:8b (the `none` rows reused from R8 run 1) and
qwen3:8b, 400 in all. Binary built from 278c7fb.

```
SLOP_BIN=target/release/slop python3 bench/archive/b5_bench.py prepare --repo flask=PATH:25 --repo httpx=PATH:25 \
  [--repo vigil=PATH:50] --r8-tasks R8/tasks.jsonl --out DIR
python3 bench/archive/b5_bench.py generate --tasks R8/tasks.jsonl --model qwen3:8b --out DIR/sketches.jsonl   # GPU host
SLOP_BIN=target/release/slop python3 bench/archive/b5_bench.py score --tasks DIR/tasks.jsonl --sketches DIR/sketches.jsonl
```

### Result (second ledger entry in `bench/results/bind.jsonl`): **G1 fails**

**G1 fails, pooled and on vigil.** bind trails the envelope on recall@5 and
is nowhere near 30% precision. The roadmap stops at G1 for re-planning.

vigil (github.com/Vigil-SOC/vigil) is public; the no-memorisation argument
holds because both models predate it. No public commit reproduces R8's
sample: at 96fe466b (nearest R8) 0 of 50 ids recur; the best commit,
f61607df (2026-08-21), has 46 of R8's 50 prompts byte for byte. vigil is
scored there with R8's ids pinned, keeping those 46 (all with R8's exact
callees): 96 tasks, 384 sketches (16 vigil sketches unscored). This is
pre-registration change 3, a labelled deviation; the G1 verdict rests on it.
The first ledger entry (flask + httpx only, "undecided") is superseded.

Pooled, 96 tasks; 95% CIs, task-cluster bootstrap; tokens spent in brackets:

| arm | recall@5 | precision@5 | cross-file recall@5 | recall@1k | @2k | @4k |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| bind | 12.7% [8.4, 17.2] | 6.9% [4.8, 9.2] | **12.9%** [8.3, 17.8] | 12.8% (622) | 17.5% (1,107) | 20.0% (1,751) |
| envelope | 21.4% [14.3, 28.7] | 8.4% [5.6, 11.2] | 2.3% [0.0, 6.9] | **41.9%** (942) | **51.5%** (1,752) | **59.4%** (3,054) |
| bm25 | 21.4% [14.8, 27.9] | 8.1% [5.8, 10.6] | 13.2% [6.2, 21.4] | 19.6% (975) | 27.6% (1,973) | 33.5% (3,965) |
| file | 25.1% [17.3, 32.3] | 11.2% [7.8, 14.7] | 0.0% | 27.1% (925) | 34.7% (1,761) | 44.1% (2,774) |
| bind+file (exploratory) | 25.6% [17.8, 33.0] | 10.9% [7.6, 14.3] | 1.9% | 28.2% (962) | 36.9% (1,890) | 51.2% (3,388) |

| G1 criterion | needed | pooled (96) | vigil (46) |
| --- | --- | --- | --- |
| bind − envelope recall@5 | >= +10, CI above 0 | **−8.6 [−16.6, −0.8]** fail | **−11.4 [−22.3, +0.3]** fail |
| bind precision@5 | >= 30% | **6.9%** fail | **7.2%** fail |
| per prompt `none` / `sketch` (pooled) | | −6.8 / −10.4 | |
| per model llama3.1 / qwen3 (pooled) | | −6.8 / −10.4 | |
| flask / httpx alone | | −5.3 / −6.8 | |

vigil alone: bind 14.5% recall@5 vs envelope 26.0%, file 33.6%; cross-file
+12.3 [+5.7, +19.4]; at 1k/2k/4k bind 14.3/21.6/26.5% vs envelope
46.1/53.2/58.9%.

Bind latency (warm, one MCP process per repo, 361 parsed sketches): p50 15.9
ms, p95 42.6 ms. E-sound fired 0 times. Unparseable: 23 of 384.

What it says:

- **The sketch names too little to bind.** On flask + httpx, parsed sketches
  make 2.3 (none) and 1.6 (sketch) candidate calls on average; 45 of 92
  parsed `sketch` prompts produced no candidate at all. The prompt meant to
  elicit helper names elicited fewer, so `sketch` is worse than `none`.
- **Only name resolution earns hits** (flask + httpx): of bind's top-5 items,
  name resolution placed 283 with 35 gold, BM25 189 with 10, callee overlap 2
  with 0. Prewrite's 0.125 overlap floor almost never passes for a 2-call
  sketch, and overlap ranks homes, not helpers, as declared.
- **The one place bind wins is cross-file**: +10.6 [+5.9, +15.3] pooled,
  because a resolved name reaches a helper in another file that the envelope
  for a gutted target (no outgoing edges) does not rank.
- **At budgets the envelope dominates** (−29 to −39 points pooled): bind
  returns short lists and has nothing more to show.
- **Same-file context is the strongest cheap arm.** `file` beats the envelope
  at 5 (25.1% vs 21.4%), and bind+file beats it on vigil (+8.6 [+1.6, +16.8],
  exploratory, not a G1 result).

Secondary check, vigil duplicates at f61607df (`b5_bench.py dupes`): the four
slugify copies (`slugify` x2, `_slugify` x2), the darktrace/cloudflare webhook
`_get_secret`, `_verify_signature` and `_parse_json`, and `_is_dev_mode` x2;
`_env_bool` does not exist at this commit. Each copy fed as the sketch:
**0 of 20 other copies rank in bind's top 5**, whether the destination is a
new file beside the copy or the copy's own file. Binder v0 binds the calls a
sketch makes, not the function it defines; the copies are not E-sound equal
to each other (E-sound only re-found the fed copy itself, which is the
excluded target). So the binder as specified cannot catch a re-written
helper; that is a duplicate-detection job, not a bind one.

Caveats: vigil is 46 pinned tasks at a commit older than R8's checkout, not
R8's own sample. flask and httpx may be memorised, which should have helped
bind. Gold includes callees nested inside the gutted target, which no arm can
return. Two 7-8B models, one sample each.

## B5b, vocabulary-primed sketches (gate G1b)

Pre-registered in `bench/archive/B5B_PREREGISTRATION.md` (a9ae027) before any
generation. Question: B5's blind sketches name about 2 helpers; if the model
sees only the names the repository already has near the target (same file,
then same directory, alphabetical, at most 400 tokens, no signatures or
bodies), does binding its sketch beat the envelope, and beat ranking that
same vocabulary by BM25 with no sketch (`vocab-bm25`, the control)? B5's 96
tasks (vigil pinned at f61607df), R8's `none` prompt plus the name block,
llama3.1:8b and qwen3:8b on matt-pc: 192 sketches. Binder v0 unchanged.

```
python3 bench/archive/b5_bench.py vocab --tasks DIR/tasks.jsonl --out DIR/tasks_b5b.jsonl
python3 bench/archive/b5_bench.py generate --tasks DIR/tasks_b5b.jsonl --model qwen3:8b --prompts vocab --out DIR/sketches_b5b.jsonl   # GPU host
SLOP_BIN=target/release/slop python3 bench/archive/b5_bench.py score --tasks DIR/tasks_b5b.jsonl --sketches DIR/sketches_b5b.jsonl
```

### Result (third ledger entry in `bench/results/bind.jsonl`): **G1b fails**

**G1b fails, pooled and on vigil, on all three criteria.** Priming lifts bind
from B5's 12.7% to 21.4% recall@5, which only ties the envelope, and the
vocabulary ranked with no sketch does as well or better.

Pooled, 96 tasks; 95% CIs, task-cluster bootstrap; tokens spent in brackets:

| arm | recall@5 | precision@5 | cross-file recall@5 | recall@1k | @2k | @4k |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| bind (primed sketch) | 21.4% [15.8, 27.0] | 13.4% [9.5, 17.5] | **17.6%** [11.2, 24.6] | 21.7% (654) | 24.5% (1,105) | 25.9% (1,618) |
| envelope | 21.4% [14.3, 28.7] | 8.4% [5.6, 11.2] | 2.3% [0.0, 6.9] | **41.9%** (942) | **51.5%** (1,752) | **59.4%** (3,054) |
| vocab-bm25 (control) | 23.1% [16.1, 30.3] | 13.7% [9.4, 18.5] | 7.3% [2.3, 13.4] | 22.9% (836) | 29.5% (1,505) | 34.5% (2,583) |
| file | 25.1% [17.3, 32.3] | 11.2% [7.8, 14.7] | 0.0% | 27.1% (925) | 34.7% (1,761) | 44.1% (2,774) |
| bind+file (exploratory) | 25.9% [18.2, 33.4] | 11.3% [7.9, 14.9] | 2.9% | 28.8% (955) | 37.7% (1,871) | 52.0% (3,349) |

| G1b criterion | needed | pooled (96) | vigil (46) |
| --- | --- | --- | --- |
| bind − envelope recall@5 | >= +10, CI above 0 | **+0.0 [−9.1, +9.0]** fail | **+0.4 [−12.8, +13.7]** fail |
| bind precision@5 | >= 30% | **13.4%** fail | **16.5%** fail |
| bind − vocab-bm25 recall@5 | > 0, CI above 0 | **−1.7 [−9.5, +6.0]** fail | **−2.6 [−14.8, +10.3]** fail |
| per model llama3.1 / qwen3 (vs envelope) | | −0.2 / +0.2 | |
| flask / httpx bind recall@5 | | 18.0% / 15.7% | |

vigil alone: bind 26.3% recall@5 vs envelope 26.0%, vocab-bm25 29.0%, file
33.6%; cross-file +19.8 [+9.9, +31.6] over the envelope.

Calls in parsed sketches (184 parsed, 8 unparseable): **3.2 candidate calls
per sketch** (B5 `none` on flask + httpx: 2.3; here flask 2.8, httpx 1.6,
vigil 4.4), of which 1.8 resolve to a repository entity. Of all calls, 29.6%
name a vocabulary entry, 26.3% another repository entity, 10.4% an imported
name, and 33.7% are invented. Bind latency p50 16.0 ms, p95 32.8 ms; E-sound
fired 0 times.

What it says:

- **The vocabulary does the work, not the sketch.** Ranking the shown names
  against the stub, with no model, matches bind on recall and precision at 5
  and beats it at every budget. The model copies about one name from the list
  per sketch; the binder resolves it; a BM25 over the list finds the same one.
- **Precision stays far from 30%.** Even primed, a sketch's resolved calls
  fill under two of the five slots with gold.
- **Cross-file is still bind's only win** (+15.3 over the envelope, +10.2
  [+1.8, +19.2] over vocab-bm25): names outside the vocabulary that the model
  writes and name resolution finds elsewhere. That gain is real but small in
  absolute terms (cross-file gold is a minority of callees).
- **Same-file context remains the cheapest strong arm**; at token budgets the
  envelope dominates every list-based arm, as in B5.

Caveats: as B5 (vigil pinned, two 7-8B models, one sample each, flask and
httpx possibly memorised). One post-generation change, a crash fix in the
call-bucket bookkeeping, is listed in the pre-registration; the ledger entry
is marked dirty because that fix was uncommitted when it was scored.

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
