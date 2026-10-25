# B5 pre-registration: does binding a sketch surface the helpers better than the envelope?

Written and committed before any generation or scoring (roadmap phase P1,
gate G1; spec `docs/prd/P1-b5-bind-recall.md`). Changes after the first
generation are listed at the end with their reason, never made silently. The
G1 thresholds below are fixed by this commit.

## Question

Given a sketch of a function written without repository context, does
matching the sketch against the repository (bind) surface the helpers the
real implementation calls better than retrieving context for the target
before any code exists (the envelope, today's product)?

## Tasks

- R8's 100 targets: 50 vigil (primary; public, see
  change 3), 25 flask, 25 httpx, selected
  by `bench/reuse_bench.py prepare` (seed 20260923, R8's rule), gutted to
  signature, docstring and `raise NotImplementedError` in a reindexed copy.
  Gold: each original's repository-internal callees (functions and classes,
  outside tests, target excluded), exactly R8's `callees` field.
- R8's ledger does not record the flask and httpx commits, so the clones are
  the default branch as it stood on 2026-09-23: flask `d73fa1cd` (main,
  2026-09-08), httpx `b5addb64` (master, 2026-02-23). Regenerating R8's
  selection on them reproduces R8's archived tasks exactly: same 50 ids, same
  callees, byte-identical `none` prompts (archive `tasks.jsonl`, sha256
  `f0660a0d…e9af2`, from matt-pc `~/r8`).
- vigil: the checkout at `~/Documents/GitHub/vigil` is not present on the
  scoring machine at the time of writing, and no copy was found on matt-pc.
  vigil sketches are generated from R8's archived prompts (which need no
  checkout). vigil is scored only once a checkout is restored on which R8's
  selection reproduces the archived 50 ids and callees, as flask and httpx
  did; otherwise vigil is reported as not scored and G1 as undecided.

## Sketches (the queries)

- Two prompts, both with R8's header (repository, file, the file's imports)
  and the gutted stub, nothing else from the repository:
  - `none`: R8's prompt, verbatim ("Implement the body of the function below...").
  - `sketch`: the same prompt with the instruction replaced by "Write this
    function as a sketch: call helpers by the names you would expect them to
    have, and use `...` for parts you would look up. Reply with the complete
    function, signature included, in a single ```python code block and
    nothing else."
- Models: qwen3:8b and llama3.1:8b on matt-pc (Ollama 0.34.3, RTX 2080),
  temperature 0, seed 0, 8,192-token context, at most 768 output tokens,
  reasoning traces off. One sample per (task, prompt, model): 400 sketches.
- Reuse: R8 run 1 generated the `none` prompt with llama3.1:8b under these
  settings against the identical prompts; those 100 responses are reused, not
  regenerated. The other 300 are generated.
- Extraction is R8's (first fenced block, dedented, `ast.parse`, the def named
  like the target, else the first def). Unparseable sketches bind to nothing
  and score zero recall and zero precision; they are counted, not dropped.

## Binder v0 (`bench/b5_bind.py`)

Deterministic; no fitted weights. For one sketch, destined for the target's
file in the gutted copy, four ranked lists are merged by reciprocal rank
(k = 60), ties broken by entity id. The target itself is never returned.
Candidates are catalog entities (functions and classes outside tests).

1. **Name resolution.** Calls in the sketch, in source order: bare names not
   bound inside the sketch and not builtins, and attribute calls whose
   receiver is not a module imported at the top of the target's file. Each
   binds to every entity with that exact simple name; a call with no exact
   match binds by name normalised to lowercase with `_` removed. Among
   several entities for one name: same file first, then same directory, then
   entity id.
2. **Callee overlap.** The MCP tool `assess_write` with the sketch as content
   and the target's file as destination, limit 10, on one warm `slop mcp`
   process per repository: `reuse_suggestions` in its order. This is
   `slop suggest`'s ranking (`Neighborhood::matches`) with prewrite's 0.125
   score floor; chosen over the CLI because it runs warm and returns JSON.
   It ranks functions whose callees overlap the sketch (homes, not helpers),
   which is declared as a known limit below.
3. **Name similarity.** For each call left unresolved by signal 1, BM25
   (k1 1.2, b 0.75) of its name's word tokens against each entity's name
   tokens plus docstring words; each unresolved call contributes its own
   top-10 list to the merge.
4. **E-sound equality.** `assess_write`'s `duplicates` for the sketch's
   functions. Expected to rarely fire; fires are counted and reported.

## Arms (matched on tokens shown)

| arm | input | ranking | cost per item |
| --- | --- | --- | --- |
| bind | the sketch | binder v0 | body excerpt (at most 30 lines) plus one call site, as `prewrite::steering` shows them |
| envelope | the target | envelope item score, adaptive default (p >= 0.01), built per budget | the envelope's own item tokens |
| bm25 | target signature and docstring | R8's BM25 over skeletons | as bind |
| file | target's file | same-file entities by line distance from the target | as bind |
| bind+file | sketch, then file | same-file entities first (file order), then bind's (exploratory) | as bind |

Binary pinned at 278c7fb (`cargo build --release -p slop-cli`), path and
sha256 in the manifest. Tokens are chars / 4 + 1. At token budgets 1k, 2k
and 4k, each list is packed in rank order, each item kept if it still fits
(R8's rule); the envelope is built at each budget and keeps its own selection.

## Metrics

- **Primary: recall@5**, the share of gold callees among an arm's top 5
  entities (the envelope's top 5 by score from its 4k build).
- **Precision@5**: gold hits among the top 5 divided by the number shown
  (at most 5); an empty list scores 0.
- Recall at each token budget; cross-file recall@5 (gold callees outside the
  target's file, on tasks that have one).
- Bind latency per sketch, p50 and p95, warm (after one warm-up call per
  repository), wall clock including the MCP round trip.
- E-sound fires; unparseable sketches per prompt and model.

## Analysis and gate G1

Paired differences against the envelope, bootstrap with the task as the
cluster (the sampled unit; all of a task's sketches are resampled together;
the B4 commit-cluster bootstrap with one target per cluster), 1,000 rounds,
seed 20260923. Per task the bind value is the mean over its sketches in the
scope; the envelope value does not depend on the sketch. The G1 scope pools
both prompts and both models (400 sketches); per prompt and per model are
reported.

**G1 passes only if both hold on the pooled set (all three repositories) and
on vigil alone:**

- bind recall@5 minus envelope recall@5 at least +10 points, with the paired
  95% interval excluding 0;
- bind precision@5 at least 30%.

Outcome stated as pass, fail, or mixed (for example pooled pass, vigil fail),
without softening. A fail stops the roadmap at G1 for re-planning. No change
to the gate or the binder after seeing results; if v0 fails narrowly, one
fitted variant may be reported as exploratory, labelled as such, never as
the G1 decision. `bind+file` is exploratory.

Secondary (does not decide G1): binder v0 on the confirmed vigil duplicates
(four `slugify`, `_env_bool` x4, the cloudflare/darktrace webhook helpers),
one copy as the sketch; reported is whether each other copy ranks in the top 5.

## Known limits, declared now

- Callee overlap ranks homes (functions that do similar work), which are
  gold only when the real implementation calls a sibling; it can cost
  precision.
- Gold is the original's resolved callees; a sketch can name a different
  helper that would also work, which counts as a miss.
- The envelope's input is the gutted target, which no longer calls anything,
  so its edit zone holds callers and the file, not callees: that is the
  product as it runs before any code exists, which is the comparison asked.
- Two 7-8B models; one sample each. flask and httpx are public and possibly
  memorised, which helps bind (better names) and not the envelope.

## Changes after the first generation

1. **Imported names skip BM25 (binder; made before any sketch was scored or
   read).** A hand-written smoke sketch of flask's `with_appcontext` sent
   `update_wrapper` (imported from functools at the top of `cli.py`) to
   name similarity, which ranked an unrelated `update` view first. A name
   the target's file imports says where it lives, so it now binds by name
   resolution or not at all. Rule 1 is otherwise unchanged.
2. **BM25 candidate set.** The bm25 arm ranks the same candidates as every
   other arm (catalog functions and classes outside tests), not R8's whole
   gutted catalog including tests. Decided while writing `prepare`, before
   scoring.
3. **vigil is public, and is scored on pinned targets (after flask and httpx
   were scored, before any vigil scoring).** vigil is github.com/Vigil-SOC/vigil,
   public; the no-memorisation argument stands because both models predate
   it. No public commit reproduces R8's sample: at 96fe466b (nearest R8,
   2026-09-23) 0 of 50 ids recur, with R8's binary (7e52daa) or the pinned
   one; three of R8's files were deleted on main on 2026-09-21, so R8's local
   checkout was older. Scanning every commit from 2026-07-01 to 2026-09-24
   for R8's stubs and imports, the best is f61607df (2026-08-21), where 46 of
   R8's 50 prompts recur byte for byte; R8's selection there still diverges
   because the other four perturb the seeded shuffle. So vigil is prepared at
   f61607df with R8's archived ids pinned (`prepare --pin vigil`), keeping the
   46 whose prompts are identical; all 46 also have R8's exact callees. The
   pooled set is 96 tasks. Under rule "vigil is scored only if R8's selection
   reproduces", the registered verdict would stay undecided; this deviation
   is what the G1 verdict below rests on, and is labelled so.
