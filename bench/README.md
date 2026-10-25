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

## `b6_replay.py`: B6, the Stop gate replayed over Claude-written commits

Pre-registered in `bench/B6_PREREGISTRATION.md` (115dcbe) before any corpus
run; three later changes are listed there (two to the harness, one
exploratory rerun). Question: how often does the Stop reconcile gate fire on
real Claude-written commits, and what share of its firings are right?

Unit: a commit with a `Co-Authored-By: Claude` trailer, one parent, and at
least one `.py` file changed: 138 units across four of the owner's repos,
claude-skills capped at 60 (seed 20261025). Per unit: index the parent,
SessionStart, check out the commit, Stop, then `reconcile` over MCP. Binary
built at 9dd2d80 (crates last changed in 9df41f4, clean). No repo has a
`slop.toml`, and `slop init` never marks a channel confirmed (it proposed one
or two in 53 primary units), so `infra-bypass` could not fire anywhere.

```
cargo build --release -p slop-cli
python3 bench/b6_replay.py run --repo claude-skills=PATH --repo gen-ai-privacy=PATH --repo LatexUEA=PATH --repo hiring-agent=PATH
python3 bench/b6_replay.py score
```

### Result (`bench/results/replay.jsonl`, labels in `bench/results/b6_labels.jsonl`)

| repo | units | unscorable | units blocked | duplicate | missing | infra-bypass | block precision | advisory precision | Stop p50 / p95 ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- | --- | --- |
| claude-skills | 60 | 1 | 10 of 59 | 374 | 0 | 0 | 0 / 374 | 0 / 13 | 117 / 483 |
| gen-ai-privacy | 46 | 42 | 0 of 4 | 0 | 0 | 0 | n/a | n/a | 73 / 287 |
| LatexUEA | 28 | 1 | 0 of 27 | 0 | 0 | 0 | n/a | n/a | 114 / 166 |
| hiring-agent | 4 | 0 | 0 of 4 | 0 | 0 | 0 | n/a | n/a | 40 / 100 |
| **all** | **138** | **44** | **10 of 94 (11%)** | **374** | **0** | **0** | **0%** | **0%** | **113 / 394** |
| gen-ai-privacy, partial index (exploratory) | 46 | 1 | 2 of 45 | 0 | 4 | 0 | 0 / 4 | n/a | 126 / 1,108 |

- **Blocks (374): 0 TRUE, 4 fact-true-but-useless, 370 FALSE, 0 UNSURE.**
  Advisory (all 13 items, fewer than the 40 cap): 0 TRUE, 9 useless, 4 FALSE.
  The exploratory `missing` findings: 0 TRUE, 4 useless.
- Unscorable, 44: 42 gen-ai-privacy units where `slop index` exits 1 because
  rust-analyzer cannot index the repo's Rust crates (pinned 1.90 toolchain
  has no rust-analyzer; stable fails too); 1 claude-skills checkout that
  collides on case (`.github/PULL_REQUEST_TEMPLATE.md` and
  `pull_request_template.md` on macOS); 1 LatexUEA unit where `slop mcp`
  closed its pipe mid-reconcile (a rerun returned 0 findings).
- Latency: index p50 9.4 s; SessionStart p50 185 ms, p95 917 ms; reconcile p50
  9.8 s (it runs the full detector suite) and hit its own index race 9 times.
  Stop stays under the 500 ms p95 budget in the primary run (worst repo 483
  ms) and misses it on gen-ai-privacy's 283-file reorg commits (1.1 s).
- Wall clock: 51 min primary, 16 min exploratory, one M-series laptop.
- "Fixed later" is uninformative here: 372 of 374 flagged lines sit in files a
  later commit removed, because claude-skills was reorganised again.

Examples (file:line at the unit's commit):

| unit | file:line | rule | label | why |
| --- | --- | --- | --- | --- |
| claude-skills bbba25a8 | `marketing-skill/social-media-analyzer/scripts/analyze_performance.py:21` | duplicate | FALSE | pure move `{ => scripts}/`; the "existing" original is the old path this commit deleted. 7 units, 363 blocks are this. |
| claude-skills 4b7a084e | `product-team/code-to-prd/scripts/codebase_analyzer.py:235` | duplicate | FALSE | `frontend_analyzer.py` renamed and extended; 7 blocks and 1 advice point at the deleted name |
| claude-skills 9dd6fd18 | `engineering/skills/slo-architect/scripts/slo_review.py:30` | duplicate | USELESS | the best catch: `_read` is byte-identical to the kubernetes-operator skill's, but skills ship standalone. The same commit adds the whole skill twice and slop says nothing, because both copies are new. |
| claude-skills 87f3a007 | `engineering-team/gcp-cloud-architect/scripts/cost_optimizer.py:27` | duplicate | USELESS | same body as the AWS skill's method, but it dispatches on `self` to GCP-specific helpers; "call the existing one" would run AWS logic, from an unimportable hyphenated path |
| gen-ai-privacy 95f37a81 (exploratory) | `investigations/03-generated-tools-lit-survey/examples/03-codeact.py:16` | missing | USELESS | `goto` is an ALFWorld tool in a verbatim dataset specimen; re-flagged only because a reorg moved the file |
| claude-skills bb6f2fa8 | `engineering-team/senior-computer-vision/scripts/inference_optimizer.py:411` | advisory | FALSE | a dict lookup of speedups "has the same shape" as a dict lookup of test frameworks |

Slop bugs found (recorded, not fixed):

1. **A moved or renamed file blocks as a copy of itself.** `session::changes`
   skips paths that no longer exist, so a deleted original never enters
   `parsed`, and `reconcile`'s `live` check (`parsed.get(file).is_none_or(..)`)
   then treats the sidecar's site in the deleted file as alive. Every function
   in a moved file becomes a "provable" duplicate. This is 370 of 374 blocks
   and 8 of 10 blocked units. The same blind spot makes `missing` re-report old
   calls in a moved file (it has no `before`).
2. **`reconcile` over MCP races its own refresher.** The tool's first capture
   queues a background reindex that rewrites the index while the same call's
   `check::run` captures again: "an index was rewritten while capturing
   analysis; retry the command" on the first edit-bearing call. The harness
   retries (9 primary, 1 exploratory); the broken-pipe unit likely shares the cause.
3. **One failing indexer silently disables duplicate detection.** With Rust
   unindexable, the snapshot is stale, SessionStart refuses to persist the
   sidecar, and Stop runs `missing` only. The only trace is a line in
   `.slop/hooks.jsonl`; `slop index` exits 1 although the Python artifact is fine.
4. Two copies of one function share one `duplicate:<hash>` id, so a single
   `acknowledge` silences both; and the action names calls across hyphenated,
   unimportable skill directories.

**Verdict: on this corpus the Stop gate has no demonstrated utility and is
net harmful.** It blocked 11% of scorable Claude commits and was right, in
the sense of "acting on it improves the code", zero times out of 374.
99% of its blocks are a bug (file moves), and the remaining 1% are true
but useless in a repo whose unit of distribution is a self-contained skill.
`missing` never fired on primary units, and on 45 more exploratory units it
fired only on quoted dataset specimens. `infra-bypass` was unreachable
without a confirmed policy. Advice was never worth reading. Strip out the
move bug and the gate would have blocked 2 of 94 commits, both wrongly in
practice. So the honest reading is that Claude's committed Python here
contains no provable mistakes of the three kinds for the gate to catch. The
caveat that bounds this: commits are final states, so mistakes Claude made
and fixed mid-session are invisible, and claude-skills is a copy-by-design
monorepo, the worst case for a duplicate check. Before any live trial
(roadmap P4), bug 1 must be fixed and the duplicate rule needs a package
boundary; otherwise the first `git mv` in a session costs a turn per
function moved.

## `b6_replay.py b7`: B7, known vigil copies replayed through Stop

Pre-registered in `B7_PREREGISTRATION.md` (committed a33dfdf, before any gate
run). Vigil-SOC/vigil at `fbf68f87`. There are 10 positives: copies of
`slugify`, `_env_bool`, the three webhook helpers and `_is_dev_mode`, each
written after another member of its group already existed. The precision
sample is 60 seeded one-parent `.py` commits. Arm A is `gate-fixes`
(f4686f2) and arm B is `equiv-nodoc` (a33dfdf; its slop code equals 290243c,
which adds the docstring-blind E-sound hash). Each parent is indexed once by
A and restored for B. The run took 194 min (11618 s) for 68 units x 2 arms.
None were unscorable. Stop p50 was 242 ms (A) and 257 ms (B), and `slop index`
p50 was 35 s.

```
python3 bench/b6_replay.py b7 --arm A:gate-fixes:/tmp/b7/bin/slop-A --arm B:equiv-nodoc:/tmp/b7/bin/slop-B
python3 bench/b6_replay.py b7-score
```

### Result (last `"bench": "B7"` entry in `bench/results/replay.jsonl`, labels in `bench/results/b7_labels.jsonl`): **park the gate**

| group | file | commit | arm A hit | arm B hit | advisory hit (A/B) | why missed |
| --- | --- | --- | --- | --- | --- | --- |
| env_bool | backend/middleware/security_headers.py | c05e4e27 | no | no | yes / yes | not equivalent: the truthy tuple is in a different order and `default` has a default; listed only as "same shape" advice |
| env_bool | backend/middleware/csrf.py | 64ea444b | yes | yes | no / no | |
| env_bool | backend/services/auth_cookies.py | 165849f7 | yes | yes | no / no | |
| get_secret | backend/api/cloudflare_webhooks.py | 66e09911 | no | no | no / no | different body (secrets manager, then env); no shape or callee overlap |
| verify_signature | backend/api/cloudflare_webhooks.py | 66e09911 | no | no | no / no | two `if` guards merged into one `or`: same behaviour, different statements, and E-sound does not normalise control flow |
| parse_json | backend/api/cloudflare_webhooks.py | 66e09911 | no | no | no / no | unverified: probably the local `import json` statement in the original (the copy uses the module import and renames `e` to `exc`) |
| is_dev_mode | backend/services/auth_service.py | 91f40aa6 | no | no | no / no | different predicate (`== "true"` vs `in ("true", "1", "yes")`) and different default |
| slugify | backend/services/custom_agent_service.py | bcc689f1 | no | no | no / no | different algorithm (regex vs `str.replace` chain) |
| slugify | services/custom_workflow_service.py | 454873c1 | no | no | no / no | different algorithm, and a `[:60]` cap and fallback |
| slugify | backend/api/llm_providers.py | d82f75e0 | no | no | no / no | regex like the others, but different fallback literal and no docstring; none of the earlier three is behaviour-equal |

Recall is **2/10 (20%) in both arms**. Advisory recall is 1/10 in both arms.
The docstring arm changed no positive: of the 10 positives, the 8 misses
differ in behaviour or in statements, not in docstrings.

| arm | block findings | TRUE | USELESS | FALSE | UNSURE | precision (all) | precision (sample only) |
| --- | ---: | ---: | ---: | ---: | ---: | --- | --- |
| A | 3 | 2 | 0 | 1 | 0 | 2/3 = 67% | 0/1 = 0% |
| B | 4 | 2 | 1 | 1 | 0 | 2/4 = 50% | 0/2 = 0% |

Each arm blocked 3 of 68 units. The only sample block (88434dd5, medic trust
statements) is the only block outside the known copies:

- **`_sub`: FALSE in both arms.** It is text-identical to
  `pack_build._sub`, but each reads its own module global `_SCHEMA`, and the
  two load different schema files. "Provably equivalent (same behaviour for
  every input)" is wrong. The E-sound hash treats a free module-level name as
  equal across modules, which is a soundness bug. It is reported, not fixed.
- **`canonical`: USELESS, arm B only.** It equals
  `medic_last_seen.snapshot_bytes` once the docstring is ignored. Acting on it
  would import `core/platform` into the separately shipped medic contracts
  package for one `json.dumps` line.

**Verdict: park the gate.** Arm B blocks 20% of positives, well under the 50%
bar. Precision exactly meets 50%, but only because 2 of the 4 block findings
are the known copies themselves. On the 60 ordinary commits it is 0/2, and
one of those two is unsound. The duplicate rule catches only text-identical
copies. The copying that actually happens in vigil (re-typed helpers with
reordered literals, merged guards, a different fallback string) is beyond
E-sound by construction, and the docstring change does not move it.
"Same shape" advice caught one more copy (c05e4e27). That points to advice,
not blocking, as the useful channel, but advisory recall of 1/10 does not
justify keeping it either.

Found on the way, and fixed before the run because it would have zeroed B7:
vigil tracks a symlinked `tools/splunk.py`. The source walk skips it but
scip-python indexes it, so every capture was stale and SessionStart never
wrote the sidecar (f4686f2).

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
