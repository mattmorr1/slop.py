# Orbit / slop — handoff, learnings, roadmap and desired outcomes

Prepared 2026-09-23 for the next engineering agent and for review (Astra).
Workspace: `/Users/matthewmorris/Documents/GitHub/slop.py`. Continues
`ideation/EXECUTION_PLAN.md`, `ideation/MODULARITY_ENGINE.md` and
`ideation/DEFERRED_RESEARCH.md`; does not replace them.

Read order: §1 state, §2 what was built and the evidence, §3 learnings, §4
process rules, §6 roadmap, §7 desired outcomes. Domain terms are defined in
`CONTEXT.md`; decisions in `docs/adr/0001`–`0004`.

---

## 1. State at handoff

### Authorisation and constraints (unchanged)

- Local commits only, one concern per commit, with rationale and measured
  numbers. **No push, tag, release or crates.io publication** (publishing is
  parked by the user).
- The user-owned working-tree entries stay untouched: the deleted
  `bench/__pycache__/compression_bench.cpython-313.pyc`, the deleted
  `editors/vscode/.vscodeignore`, untracked `.DS_Store` files.
- User's global rules (`~/.claude/CLAUDE.md`): ExecutionPlan + option matrix
  before major data-structure / concurrency / network / topology choices;
  comments ≤ 2 lines; no placeholders; no swallowed errors; semantic audit and
  lint/test before declaring done; `pal` for ad-hoc scripts (`pal
  report-missing` before abandoning it). Benchmark harnesses are Python by
  repository convention.
- User choices this program: equivalence engine **A3 egglog** (kept as oracle,
  see ADR 0004); compression **B1** (dedup + budgeted selection + `expand`);
  benchmark budget **$0** (model-free metrics and local models only).

### Commits (all local, on `main`, after `e86ae58`)

| commit | what |
| --- | --- |
| bcf7691 | ADRs 0002/0003 and domain docs for the integrated tree |
| 8cd40b4 | the integrated harness (prior agent's dirty tree, landed) |
| 2599d9c | packaging/publishing, parked |
| 3fe14d6 | snapshot: content-true identity, per-document SCIP coverage |
| 8b0d6b1 | parse: TSX grammar, scope-correct JS/Rust bindings, 2.9x faster cold capture |
| f7150bc | parse: α v2 for Python (AST scopes, signature hashed) |
| b09232e | equiv: E-equivalence (normalizer + egglog behind `egraph` feature), B3 bench |
| 6358902 | bench: manifest dirtiness excludes caches/ledger |
| cd9af44 | equiv: graded confluence + termination fixes the ablation exposed |
| 1434d74 | equiv: locals numbered in the normal form |
| c7f24a9 | ADR 0004 + CONTEXT.md grades + B3 ledger |
| c19cf71 | detect: `duplicate-equivalent`; `async`/annotations in the E-signature |
| 0a297c2 | bench: B3 run c19cf71 recorded |
| 467b153 | build: entity names drop SCIP descriptor decoration |
| 1755072 | snapshot: identity is location-independent |
| 62a583f | envelope: coverage selection, E-class equivalents, MCP `expand_entity` |
| 66b9b5f | index: scip-python `--project-version`, TS monorepo tsconfigs |
| d54cc1f | bench: B4 context benchmark + `context-bench` adapter |

Every commit was checked to build and pass `clippy -D warnings` on its own
(bcf7691 inherits two pre-existing lints from `e86ae58`, fixed in 8cd40b4).

### Calibrated relevance port (R1, done: 7dc5255, evidence in ADR 0005)

- `relevance.rs`: logistic `RelevanceModel` (12 features, default = pooled B4
  fit, per-repo override `.slop/relevance.json`), `Lexical` BM25 per snapshot.
- `envelope.rs`: pool = graph <= 4 hops plus the target's directory; packing by
  probability per token with E-class dedup; reasons are logit contributions.
  `same_container` uses the benchmark's id-prefix definition (train/serve
  skew found and fixed; golden test pins one probability).
- B4 after-run (7dc5255, clean, same task digest as d54cc1f): shipped selector
  out of sample 66.2% at 4k vs BM25 59.3%, proximity 57.0%, old 51.4%. Rust
  equals the Python model on 21,396 probabilities; the 2.4-point gap to the
  Python arm is full-fidelity edit-zone neighbours. Level with BM25 on httpx
  and requests. vigil warm context p95 132 ms (target 50 ms), see R10.
- 299 tests pass, clippy clean.

### Scratch environment (session scratchpad, not the repo)

- vigil copy with fresh indexes: `…/scratchpad/corpus/vigil` (sources only;
  history comes from `~/Documents/GitHub/vigil`, read-only).
- OSS clones indexed by slop: `…/scratchpad/oss/{requests,flask,httpx}`.
- Pinned "before" binary: `…/scratchpad/slop-before`.
- Heavy runs may use `ssh matt-pc` (Tailscale); it timed out on 2026-09-23.

---

## 2. What was built, and the evidence

### P1 — the repository snapshot tells the truth

| defect | fix | evidence |
| --- | --- | --- |
| cache key = file count/bytes/newest mtime | content digests; git racily-clean stat tuple (size, mtime, **ctime**, inode) avoids rereads | backdated same-length edit test; warm capture vigil p50 70 → 17 ms |
| repo-wide mtime freshness | per-document content stamps; Resolved / Stale / Unresolved per document; context degrades per document | stale document test; `omitted_unresolved` in artifacts |
| vigil `check --all`: 50 s then failure | legacy root index adopted; indexer output staged + validated (0 documents = failure); failures memoised by content; stdout of indexers → stderr | vigil: 1.8 s success with explicit partial evidence |
| scip-python crash in git-less checkouts | `--project-version 0` | root cause read from the indexer log, not the misleading "Python 3.13 unsupported" warning |
| scip-typescript needs a root tsconfig | subproject tsconfig dirs passed positionally; `--infer-tsconfig` file removed after | vigil fully indexed in 51 s, 1,072/1,088 files, freshness current, findings 846 → 1,509 |
| `.tsx` parsed with the TS grammar | `Language::Tsx` | 74 vigil React files regained facts |
| cold capture 1.65 s (vigil) | parse on a work-stealing thread pool; no per-token `format!` (hashes byte-identical, golden test) | 0.55 s |
| snapshot id embedded absolute index path | repo-relative | two checkouts, one id (test) |

### P2 — equivalence you can deny on (ADR 0001, ADR 0004)

- α v1 had six real false-equivalence holes (keyword names, `global`,
  defaults, defaults reading globals, comprehension leakage, decorators),
  confirmed against the old binary with bodies above the significance floor.
- α v2: bindings from AST scopes; signature (defaults, annotations, kinds,
  decorators, `async`) hashed; comprehension/lambda-only names handled.
- E-equivalence: sound laws (hold for every Python value) vs graded laws
  (commutation, flips, De Morgan, `x += y`), a terminating normalizer, and
  bounded egglog behind `--features egraph`.
- B3 (`bench/equiv_bench.py`, commit c19cf71, 3,759 pairs from 400 real
  functions): E-sound recall **99.1 %** on provable rewrites vs α v2 21.6 %;
  **0 / 1,813** false equivalences on behaviour changes and 0 / 111 on type
  traps (rule of three < 0.17 %). Body-only hashes call 100 % of `async` and
  annotation changes equal.
- egglog found a graded confluence bug and a non-termination bug in the
  normalizer, then never proved a pair the normalizer missed; normalizer is
  25–45x faster. Shipping decision: normalizer in production, egglog as oracle.
- Product: `duplicate-equivalent` (Warning); 20 groups in vigil, e.g.
  `ElasticService.search_by_ip` ≡ `search_by_hash`.

### P3 — context: selection measured against history (B4)

`bench/context_bench.py`: tasks are leave-one-out co-change from real commits
(1,326 tasks, 4 repos: vigil, requests, flask, httpx); recall@budget; commit-
cluster bootstrap CIs; every arm charged slop's skeleton cost; no-edge subset
against graph leakage; logistic model fit leave-one-repo-out.

Canonical *before* run (pooled recall, 95 % CI):

| arm | @1k | @2k | @4k | @8k | @16k |
| --- | ---: | ---: | ---: | ---: | ---: |
| calibrated (ratio) | 50.1 % | 60.0 % | **68.6 %** [65–72] | 78.3 % | 89.6 % |
| calibrated (rank) | 48.3 % | 57.9 % | 69.5 % | 80.7 % | 90.1 % |
| BM25 | 41.9 % | 50.2 % | 59.3 % | 69.6 % | 81.3 % |
| file proximity | 40.3 % | 47.7 % | 57.0 % | 72.2 % | 83.3 % |
| slop ranked (old scorer) | 33.2 % | 41.8 % | 51.4 % | 60.3 % | 68.3 % |
| slop coverage (first objective) | 20.4 % | 29.3 % | 36.2 % | 44.3 % | 54.5 % |

Fold coefficients are stable: same file +5.9…+8.2, same directory +2.0…+4.2,
direct edge +0.5…+1.3, BM25 +0.4…+1.1 per log unit, shared effect +0.2…+0.8,
class −5…−6 (artefact: gold is functions only). On vigil alone, file
proximity still beats the cross-repo model at ≥ 2k — the case for per-repo
calibration.

Also shipped: `expand_entity` (MCP) returns an entity's verbatim source from
the same snapshot, making skeletons reversible; context items list E-equivalent
`equivalents`.

---

## 3. Learnings

### Technical

1. **Measure before optimising.** The planned SCIP-loader interning would have
   saved nothing: loading was 148 ms, parsing 1.15 s — and the parse profile
   exposed two correctness bugs (TSX, scope leak).
2. **Soundness claims need adversarial tests with teeth.** A probe "confirmed"
   three α holes on bodies below the significance floor, where both hashes
   were empty and trivially equal. Every soundness test now asserts the hash
   is non-empty, and every fix was checked red-then-green against the old code.
3. **Python's dynamic semantics make most algebra unsound.** `str +` does not
   commute, `list.__iadd__` aliases, De Morgan changes `__bool__` call counts,
   annotations are behaviour under FastAPI/pydantic, `async` changes the return.
4. **E-graphs pay only where rules are not confluent.** Sound laws with fixed
   orientations normalise; saturation added nothing. Its value was differential
   testing. A binder-less e-graph also cannot renumber locals after reaching a
   normal form; the normalizer can.
5. **Real output finds what mutators don't.** The `async`/annotation hole was
   found by reading `duplicate-equivalent` on vigil; the class-name bug by an
   `expand` round-trip test. Add a mutator for each class found.
6. **Objectives must be calibrated before they are optimised.** Budgeted
   coverage over hand-weighted units rewarded hub functions and cheap class
   stubs; the ratio greedy is only right when weights are probabilities.
7. **Co-change is dominated by locality**, then direct edges and vocabulary;
   effect overlap barely predicts it. Effects explain what code does; history
   explains what code you need. Effects belong in skeletons, locality in
   selection.
8. **Determinism is a data-structure property.** HashMap-order float sums,
   absolute paths in ids, name-dependent tie breaks and HashMap iteration in
   grouping each broke byte-identical output until fixed.

### Process (including mistakes made and their guard rails)

| mistake | guard rail now |
| --- | --- |
| `cargo fmt -p` reformatted the whole crate | never run package-wide fmt; format only files you own, or not at all |
| perl `-0pi` read another file into `$_` and emptied `lib.rs` | never read files inside an in-place perl loop; use the Edit tool for multi-line edits |
| perl `|` delimiter collided with regex `|` and prepended junk | same |
| committed with a failing test (awk summary exits 0) | test pipelines must `exit (f>0)`; check the count before committing |
| a commit that did not build alone (field added, caller in next commit) | verify every commit builds via a detached worktree loop before handing off |
| running `slop` on the user's vigil created `.slop/` there | benchmark on scratch copies; clean up any side effect you cause, and say so |
| benchmark "dirty" flag fired on user-owned deletions | manifests use `--untracked-files=no` and exclude caches/ledgers |
| rebuilding the binary under a running benchmark | pin the measured binary (`SLOP_BIN`) |

---

## 4. Process rules that worked

- Plan → option matrix → user choice at data-structure checkpoints; measured
  decision rules for sub-choices (e.g. "full hash if ≤ 15 ms p95, else
  racily-clean").
- One concern per commit; message records the defect, the fix and the number.
- Append-only JSONL ledgers (`bench/results/*.jsonl`) with manifest: commit,
  clean/dirty, seed, corpus/task digests, platform, model coefficients.
- Red-green for every correctness fix; golden tests pin persisted encodings.
- Evaluate the shipped implementation out of sample (leave-one-repo-out model
  files fed to the Rust binary), not only a Python replica.
- Claims are labelled exploratory until a pre-registered confirmatory run.

---

## 5. Known limitations (honest list)

- Provisional default relevance weights (uncommitted) — R1.
- No end-to-end model-graded result; the "harness beats competitors regardless
  of model" claim is unsupported beyond model-free metrics.
- Aider's actual repo map has not been run as a competitor arm.
- E-lowering is Python-only; JS/TS and Rust duplicate detection stops at α.
- Edits make documents Stale until reindex; no incremental reindex. First index
  8 s (small repos) to 51 s (vigil).
- Read-hook compressor (`compress.rs`) still chooses fidelity by hop count, not
  calibrated relevance.
- LSP ignores unsaved buffers; Codex host gets MCP only (no hooks).
- Deny gate (ADR 0001) is still not wired into the write hook.
- Cross-language route edges (TS `fetch` ↔ Python route) unbuilt.
- Not published; install is `cargo install --path crates/slop`.

---

## 6. Roadmap

Ordered; each item has an exit gate. Do not start an item before the previous
gate unless it is independent.

**R1 — Calibrated context port. Done (7dc5255, ADR 0005, B4 after-run).**

**R2 — Real competitor arm.** Add Aider's RepoMap (pinned version) to B4,
same budgets and cost model. *Gate:* result recorded whatever it shows.

**R3 — Per-repo calibration as a product command.** `slop calibrate` (port the
history miner or ship the script) writing `.slop/relevance.json`; time-split
validation printed. *Gate:* on vigil, calibrated-own ≥ file proximity at 4k.

**R4 — Incremental freshness.** On save, re-parse changed documents, keep
resolved evidence for unchanged ones, reindex in the background (coalesced).
*Gate:* edit-to-fresh-context p95 < 1 s on vigil without a full reindex.

**R5 — Calibrated read-hook compression.** Keep a function full when its
calibrated relevance to the edit zone clears a threshold; evaluate on a
modification task set (can the agent still edit correctly), not recall.

**R6 — Deny gate for provable duplicates.** Wire E-sound matches into the
`PreToolUse` hook behind a policy flag (ADR 0001). *Gate:* 0 false
equivalences in B3 plus two weeks of advisory logs with no false match.

**R7 — E-lowering for TypeScript** (then Rust), with B3 mutators per language.

**R8 — First end-to-end signal.** When `matt-pc` is reachable: a small paired
run (local models) on a RepoReuse-style task set: does the agent reuse the
helper history says it should? Pre-register hypotheses first.

**R9 — Daily-driver polish.** LSP unsaved-buffer overlays; Codex hook parity;
`slop doctor` checks for npx/rust-analyzer and indexer failure memos.

**R10 — Envelope latency on large repos.** vigil p95 132 ms: every call
rebuilds signature/class maps over all facts and scans every entity for the
target's directory. Build both once per snapshot (like `lexical()`), index
entities by directory. *Gate:* vigil warm context p95 < 50 ms, output
byte-identical.

**Deferred (unchanged gates in `DEFERRED_RESEARCH.md`):** e-graph code
optimisation, daemon, constrained decoding, RL/RLAIF, cross-process cache.

---

## 7. Desired outcomes

Measurable, in priority order.

1. **Trustworthy evidence.** Every judgment and context artifact names a
   content-identified snapshot; stale evidence never skeletonises or denies
   silently. *Measure:* freshness tests; zero known identity bugs.
2. **Provable duplicate prevention.** E-sound false equivalence 0 on B3 in
   every language it covers; recall ≥ 95 % on provable rewrites; deny gate on
   with no user-reported false deny in daily use.
3. **Context that beats the obvious alternatives.** Out-of-sample recall@4k at
   least 5 points above the best of {BM25, proximity, Aider repo map} pooled,
   and not below proximity on any single repo after per-repo calibration.
4. **Usable daily.** First index < 60 s on vigil; warm context p95 < 50 ms;
   edit-to-fresh p95 < 1 s (R4); install + `slop setup` in under five minutes.
5. **End-to-end benefit, stated honestly.** A pre-registered paired run showing
   whether reuse rate and introduced findings improve with the harness, across
   at least two model families; nulls reported.
6. **Explainability.** Every selected context item and every finding carries
   reasons an engineer can check (feature contributions, evidence loci).

---

## 8. How to run things

```
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --release -p slop-cli --features egraph      # equivalence bench
python3 bench/equiv_bench.py --limit 400                  # B3
cargo build --release -p slop-cli
SLOP_BIN=… python3 bench/context_bench.py \
  --repo vigil=$HOME/Documents/GitHub/vigil:<indexed vigil copy> \
  --repo requests=<clone>:<clone> --repo flask=<clone>:<clone> \
  --repo httpx=<clone>:<clone> [--fit-all <path>]          # B4
```

Bench snapshots must be scratch copies: B4 writes and removes
`.slop/relevance.json` in each snapshot and refuses to overwrite an existing one.
