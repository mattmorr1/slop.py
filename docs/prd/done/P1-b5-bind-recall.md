# PRD P1: B5, bind recall

Roadmap: `docs/roadmap.md`, phase P1 and gate G1. Runs in parallel with P0 on
its own worktree (`p1-b5`). Budget: $0 (local models on matt-pc only).

## Question

Given a sketch of a function written without repository context, does matching
the sketch against the repository (bind) surface the helpers the real
implementation calls better than retrieving context for the target before any
code exists (the envelope, today's product)?

This is the premise of the whole roadmap. If bind does not beat the envelope,
P2 onward does not start as written.

## Pre-registration first

Write `bench/B5_PREREGISTRATION.md` and commit it before any generation or
scoring, in the style of the archived `R8_PREREGISTRATION.md`: question, tasks,
arms, models, metrics, hypotheses, analysis, and a changes-after-generation
section. The thresholds below are proposals; they are fixed by that commit.

## Tasks

- R8's 100 targets: 50 vigil (private, primary), 25 flask, 25 httpx. Same seed
  (20260923) and selection rule, regenerated with the archived
  `bench/archive/reuse_bench.py prepare` (on the P0 branch it moves; on this
  branch use `bench/reuse_bench.py`). Gold: each original's
  repository-internal callees, as R8 defined them.
- vigil: `~/Documents/GitHub/vigil`. flask and httpx: fresh clones at the
  commits R8's ledger entry recorded in `bench/results/reuse.jsonl` (check the
  manifest; if absent, the commit nearest 2026-09-23 on the default branch, and
  say so in the pre-registration).

## Sketches (the queries)

- R8's `none` arm is exactly an isolated sketch: signature and docstring only.
  First look for R8's generations on matt-pc (`ssh matt-pc`; WSL needs an open
  session, see memory). If found and their task digests match, reuse them.
- Otherwise regenerate: `none` arm only, qwen3:8b and llama3.1:8b, R8's
  settings (temperature 0, seed 0, 8,192 context, 768 output tokens). Add one
  sketch-style prompt arm: "write this as a sketch: call helpers by the names
  you would expect them to have, use `...` for parts you would look up." Two
  prompts x two models x 100 tasks = 400 generations.
- Unparseable sketches are scored as zero recall, not dropped.

## Binder v0 (Python reference)

`bench/b5_bind.py`, stdlib only, calling the binary. For one sketch it returns a
ranked list of repository entities, each costed in tokens as the body excerpt
plus one call site that `prewrite::steering` would show. Signals, all
deterministic:

1. **Name resolution**: each call in the sketch that names an existing entity
   (exact simple name, then case- and separator-normalised) binds to it.
2. **Callee overlap**: `slop suggest --file <path>` with the sketch on stdin
   (existing callee-neighbourhood ranking).
3. **Name similarity**: for each unresolved call, BM25 of its name tokens
   against entity names and docstrings (`find_capability` over MCP, or a direct
   BM25 over the index's entity list if that is simpler and faster).
4. **E-sound equality** of whole sketch functions against the equivalence index
   (will rarely fire; reported separately).

Merge by reciprocal rank, ties broken by entity id. No fitted weights in v0; if
v0 passes, P2 can fit them. If v0 fails narrowly, one fitted variant is allowed
as an exploratory arm, labelled as such, never as the G1 decision.

## Arms (matched on tokens shown)

| arm | input | notes |
| --- | --- | --- |
| bind | the sketch | binder v0 |
| envelope | the target | binary pinned at 278c7fb, adaptive default |
| bm25 | the target's signature and docstring | R8's BM25 |
| file | the target's file | R8's file arm, reported as reference |
| bind+file | the sketch, plus same-file items first | hybrid, exploratory |

Each arm's list is cut to the same token budgets: 1k, 2k, 4k.

## Metrics

- Primary: recall@5 of gold callees (top 5 entities per arm), and recall at
  each token budget.
- Precision@5: share of the top 5 that are gold callees.
- Cross-file recall: gold callees outside the target's file (where R8 said
  every arm was weak).
- Bind latency per sketch, p50 and p95, warm.
- All with the commit-cluster bootstrap B4 uses (1,000 rounds), paired against
  the envelope.

## Gate G1 (proposed; fixed by the pre-registration commit)

Pass requires both, on the pooled set and on vigil alone:

- bind recall@5 minus envelope recall@5 at least +10 points, paired 95% CI
  excluding 0;
- bind precision@5 at least 30%.

Report pass, fail, or mixed (for example pooled pass, vigil fail) without
softening. A fail stops the roadmap at G1 for re-planning.

Secondary check (does not decide G1): run binder v0 on the confirmed vigil
duplicates (four `slugify`, `_env_bool` x4, the cloudflare/darktrace webhook
helpers; see memory `slop-dogfood-repos`), feeding one copy as the sketch, and
report whether the others rank in the top 5.

## Deliverables

- `bench/B5_PREREGISTRATION.md` (first commit).
- `bench/b5_bind.py`, `bench/b5_bench.py` (`prepare`, `generate`, `score`,
  same shape as R8's harness).
- A ledger entry appended to `bench/results/bind.jsonl` with the usual
  manifest (commit, dirty flag, seed, corpus digest, platform, models).
- A "B5" section in `bench/README.md`: tables, paired differences, the G1
  verdict, caveats.

## Out of scope

- Any Rust change. Binder v0 only reads the binary's output.
- Any paid model.
- Tuning the gate after seeing results.

## Constraints

- Build the pinned binary from 278c7fb in a separate worktree; record its path
  and sha in the manifest.
- vigil is private: no vigil source in committed files or the ledger, only
  ids, counts and scores.
- Step-by-step commits with rationale. No push.
- If matt-pc is unreachable for more than a few attempts, stop and ask rather
  than switching to a hosted model.
