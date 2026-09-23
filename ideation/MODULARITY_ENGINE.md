# The modularity engine — running plan

> **Status: prospective retrieval is wired into prewrite assessment; signal (1) killed.**
> `retrieve.rs` / `slop suggest` answers extend-vs-create *before* code exists —
> the original question — and found real cross-package duplication in both
> dogfood repos. Ranking is integer and deterministic; the conservative
> `125_000` squared-cosine threshold is enforced by the snapshot-bound MCP and
> hook paths. The threshold remains an evaluation parameter, not a deny rule.
>
> **Signal (1) failed its kill criterion.** Built on SCIP
> def-use (D20), tri-lingual, and adjudicated against 1184 Python functions in
> vigil plus slop.py's own Rust: **60 findings → 29 non-interleaved → 5 past a
> guard filter → ~1 worth acting on.** Not wired into `run_all`. The mechanism is
> correct; the premise was wrong. See §5(1) for the four reasons and §2 for what
> it does to D13.

This is a living document. Decisions get promoted into the decision log in
[`EXECUTION_PLAN.md`](./EXECUTION_PLAN.md); open questions and progress stay
here. Terms are defined in [`../CONTEXT.md`](../CONTEXT.md).

---

## 1. What this adds

slop judges code against the graph of what a codebase already is. It cannot yet
answer the question an agent faces *before* it writes: **does this belong in an
existing function, or a new one?** Left unanswered, a model defaults to creating
— new functions, mini-helpers, parallel implementations — and the codebase
accretes units nobody chose.

The thesis in one line: **modularity is minimum cut at every scale.** The same
computation on a different graph — statements within a function, functions within
a module. Cohesion and code volume are consequences of it, not separate
objectives to weigh (D13).

slop already encodes both failure modes, but only retrospectively:
`trivial-wrapper` punishes fragmenting too far, `complexity-spike` punishes
extending too far. There is an implicit corridor between them. This makes it
explicit, computable, and available at write time.

---

## 2. Decisions locked (rationale in EXECUTION_PLAN.md D13–D18)

| # | Decision |
| --- | --- |
| D13 | Modularity is minimum cut at every scale. One objective; cohesion and volume are consequences. **Qualified by M0 — see below.** |
| D14 | The write hook may deny, but only on a **provable** signal — currently α-equivalence alone. See [ADR 0001](../docs/adr/0001-deny-only-on-alpha-equivalence.md). |
| D15 | slop proposes boundaries and never applies a split. **Naming is the one step delegated to the model** — fuzzy, and a wrong answer is cosmetic rather than a bug. |
| D16 | Extraction requires **two** keys: MDL profitability **and** co-change evidence. |
| D17 | One repo. This is new crates in slop, not a separate tool. |
| D18 | Every hash carries a version tag before it is ever persisted. |
| D19 | **Signals are placed by cost, not by phase.** Nothing that touches the whole graph — or the index — goes near a per-write hook. |
| D20 | **Def-use comes from SCIP.** tree-sitter keeps only the line→statement partition, which SCIP does not encode. |

### Placement (D19, as amended by D20)

| Surface | Signals | Budget |
| --- | --- | --- |
| `PreToolUse` | h₂ deny; neighborhood retrieval | ~10ms. Both work on proposed text alone, with no index lookup |
| `Stop` (new event) | corpus sieve — LSH, MDL, co-change — **plus dataflow components**, scoped to the session edit zone | seconds; a reindex is affordable and a coherent unit of work exists by then |
| `slop check` / CI | full audit | unbudgeted |

Two constraints force this. Read-path compression already costs ~0.1–0.5s here
and ~1.2s on vigil, so the per-write path has no room for anything corpus-wide.
And under D20 the components signal reads the index, so it **cannot** run on a
function that was written thirty milliseconds ago — a fresh body has no
occurrences at any budget. Signal (1) therefore moves off the write path
entirely.

That is a real loss, and worth naming rather than glossing: slop cannot tell an
agent "the function you are writing is two functions" *while* it writes. It can
only say so at the end of the turn. The alternative — a tree-sitter def-use
fallback for the one edited function — means building the mechanism D20 just
declined to build, three times over, to serve a proposal that is not actionable
mid-write anyway.

The edit zone the `Stop` sieve scopes to already exists; the `PostToolUse` hook
maintains it.

### What M0 does to D13

"Modularity is minimum cut at every scale" is a statement about a graph, and it
is only as good as the edges. At the **statement** scale the graph is dataflow,
and M0 showed dataflow omits three couplings that decide separability: control
transfer, shared mutable state, and required ordering. A cut through that graph is
cheap in values and can still be illegal. Signal (1) did not fail because a
threshold was wrong — it failed because the edge set was incomplete, so the cut
was minimal in the wrong metric.

At the **module** scale the same claim rests on a different graph — `Calls` plus
`HasEffect` — which does encode effects, and where `infra-bypass` and
`effect-layer-violation` already produce findings people act on. So D13 survives
where slop has been measuring it and fails one scale down, which is the opposite
of the "scale-free" property that made it attractive.

The consequence for signal (2): min-cut runs on the *same* dataflow graph, so
three of the four failure causes transfer directly. It should not be built as
specified. Signal (3) (MDL over redundancy) and prospective retrieval are
untouched — neither ever depended on intra-procedural dataflow.

### The extraction rule (D16)

$$\text{extract} \iff \underbrace{L(F) > \tfrac{n}{n-1}L(\text{call})}_{\text{MDL: worth it}} \;\wedge\; \underbrace{P(\text{co-change}) > \tau}_{\text{semantics: same thing}}$$

The first conjunct derives the folklore rule of three rather than asserting it: at
`n=2` a fragment must be twice a call site to pay for itself, at `n=3` only 1.5×,
at `n→∞` merely larger than the call. Because `L(call)` grows with arity, the
parameter-count penalty falls out for free — no "too many arguments" lint needs
writing.

The second conjunct is what separates a tool people keep from one they disable in
week three. MDL says a fragment *pays for itself*; it cannot say two sites *mean
the same thing*. Coincidental structural twins that later diverge produce the
classic wrong-abstraction failure: you extract, then one site needs different
behaviour, you add a flag parameter, and the result is worse than the duplication
you removed.

---

## 3. Open questions

| Question | Default assumed by this plan | Cost to reverse |
| --- | --- | --- |
| **Effect row for a typed hole**: blocklist (`LayerRule.forbid`, exists today), explicit whitelist (`allow`, new config burden), or **inferred** from what the layer already does? | Inferred — the same dominant-pattern approach D8 uses for channels. A layer whose modules have only ever done `FsRead` has that as its row; a write that widens it is the finding. Makes "effect-creep at layer scale" fall out as a detector. | Low |
| **Persistence shape**: JSON ledger + rebuildable cache, or SQLite? | JSON ledger, **committed** (suppressions are decisions and should be reviewed in the PR), plus a gitignored rebuildable cache for derived indexes. SQLite earns its place when LSH lookup outgrows a linear scan — M4, not M0. | Low for JSON, high once SQLite lands |
| **Co-change granularity**: file-level (one `git log --name-only` pass, nearly free) or site-level (needs parsing each historical revision)? | File-level as the proxy, with the hole documented. | Medium |
| **Hook time budget**: the `check` hook wants <1s, but read-path compression already costs 0.1–0.5s here and ~1.2s on vigil. | Measure before adding to the hot path. | — |

---

## 4. What already exists (read this before estimating anything)

The single biggest risk to any estimate here is rebuilding what is already
built and tested (239 tests at time of writing).

| Needed | State |
| --- | --- |
| Rust binary, no daemon, no network | **exists** |
| tree-sitter frontends behind one seam | **exists** — Python (ruff), JS, TS, Rust, via `slop_parse::Language` |
| SCIP-backed call graph | **exists**, and is better than tree-sitter would give (pyright / rust-analyzer grade) |
| MCP stdio server | **exists** — `find_capability`, `validate_change`, `get_context_envelope`, `query_subgraph` |
| Hooks | **exists** — PreToolUse, PostToolUse, UserPromptSubmit, SessionStart, SubagentStart. A `Stop` event is trivial to add |
| CLI | **exists** — 15 commands incl. `init`, `check`, `gate`, `fix`, `index`, `install` |
| Skill | **exists** — `skills/slop/SKILL.md` |
| Propose-only | **exists** — D9 plus warn-only hook |
| Effect lattice + layer policy | **exists** — this is the effect-row substrate |
| **h₂ (α-equivalence hash)** | **exists** — committed, 5 properties asserted |
| Test-reachability signal | **exists** — `untested-effect`, the amplifier guard below |
| Neighborhood distinctiveness (IDF) | **exists** — inside `parallel-implementation` |
| Auditor subagent | missing, small |
| **Def-use relation** | **exists as data, discarded** — SCIP occurrences carry symbol + definition role and are already parsed on every run; `build_analysis` drops the resolver afterwards (D20) |
| **Statement partition + components** | **missing — the real work, and much smaller than a PDG per language** |
| **LSH / MinHash over WL features** | **missing** |
| **Anti-unification + MDL scoring** | **missing** |
| **Co-change mining** | **missing** |
| Persistent store / ledger | missing |

---

## 5. The signals

Ordered cheapest-first. Each is either *provable* (a fact about the code — may
deny) or *graded* (has a free parameter — advisory only). See CONTEXT.md.

**(1) Disconnected dataflow — provable, and KILLED on evidence.** The claim was
that a body forming ≥2 components with no dataflow between them is two functions
concatenated, with the boundary derivable rather than judged. Built
(`crates/slop-analyze/src/split.rs`) and measured:

| Stage | vigil (1184 functions ≥4 statements) |
| --- | --- |
| ≥2 substantial dataflow components | 60 |
| …with non-interleaved parts | 29 |
| …with no part that transfers control | 5 |
| …worth acting on, read individually | ~1 |

Four systematic reasons two statements exchange no values and still belong
together, none of which a dataflow graph can see:

- **Control coupling.** `x = fetch(id); if not x: raise` consumes its value and
  then leaves. Every guard is its own component, so idiomatic early return
  *guarantees* a hit. 31 of 60.
- **Effect coupling.** `refresh_custom_agents` clears `self.agents` then reloads
  it. Zero dataflow, and inseparable — the medium is mutation and the contract is
  ordering. Splitting invites calling one half without the other.
- **Interleaving.** Components woven through each other admit no cut at all. Half
  the raw findings.
- **Recursive traversal.** Handle this node, then recurse into children:
  disconnected by construction. This is what it found in slop.py's own Rust.

One finding was genuinely useful and still mislabelled: `enrich_event_with_context`
is two `if x: event[k] = x` blocks, which wants a loop over field names, not a
split. The signal saw repetition and proposed the wrong repair.

*Mechanism, which was not the problem.* Def-use came from SCIP occurrences (D20)
rather than a tree-sitter binder walk:

*Mechanism (D20).* The def-use relation comes from SCIP occurrences, not from a
tree-sitter binder walk:

| Concern | Source |
| --- | --- |
| which statements exist, and which lines each spans | tree-sitter, from the walk that already produces `FunctionFacts` |
| which symbols a line defines and reads | SCIP occurrence `symbol` + `is_definition` |
| is a symbol internal (dataflow) or external (interface) | `SymbolKind::Local` / `Parameter` vs everything else |
| shadowing, closure capture, comprehension scope | SCIP, correctly, for free — a distinct `local N` per binding per scope |

Two properties of this that a name-matching walk cannot have: `self.x = 1` is a
real definition of a real symbol rather than an attribute store nobody tracks,
and two same-named locals in sibling scopes never produce a false edge. Verified
present in both indexers before committing to it — vigil's scip-python index and
slop.py's own rust-analyzer index both emit `local` symbols with definition
occurrences.

Its cost is a dependency on index freshness, which is what moved the signal off
the write path above. All of it works, on all three languages; it is retained for
signal (2), which needs the same graph.

**(2) Small min-cut — graded.** When components are connected but weakly, min-cut
on the PDG. Cut size *is* the interface width of the proposed split. Split when
the cut is small relative to both sides. Same conductance maths as module
boundaries, one scale down.

**(3) MDL — graded.** Anti-unification over near-dup clusters yields
`(pattern, residual)` pairs; score with the inequality in §2.

**Co-change gate (D16)** applies to (3), and to any retrieval that proposes
reusing an existing module.

**Prospective retrieval — BUILT, and it works.** Signals 1–3 all need a body to
analyse, so none of them answers extend-vs-create *before* the code exists. This
does: take the proposed content's callee set, compare against existing functions'
neighborhoods with the IDF machinery `parallel-implementation` already has, and
name the function it belongs in. `crates/slop-analyze/src/retrieve.rs`, exposed as
`slop suggest` (stdin) and `slop suggest --eval` (leave-one-out over a repo).

Measured leave-one-out on two corpora. Three fixes, each removing a whole class of
false positive, each found by reading output rather than by reasoning:

| Fix | Why | Effect |
| --- | --- | --- |
| Only `Function`-typed callees | Constructing an enum variant is a `Calls` edge, so three functions that merely mention one enum's variants scored like three sharing real work | 21.7% → 4.1% of functions matched |
| Cosine, not raw overlap count | `slop::main` is a 19-arm dispatcher, calls everything, and outranked every real match | `main` fell to one appearance, last |
| Drop names the source *defines* | `qualified_names` reports a function's own name from its `def` line, so every caller/callee pair looked like a duplicate | vigil 560 → 508 |

Precision after all three, adjudicated by reading: **~7 of 9 unique pairs at
score ≥0.55 on vigil**, ~12 of 19 on slop.py. Above the 50% bar M0 failed, and
unlike M0 the residual false positives cluster at *low* scores, so a threshold
works — that is the structural difference between a calibratable signal and a
dead one.

What it found, which is the real evidence: vigil duplicates `get_secret` /
`set_secret` / `delete_secret`, `init_database` and `get_db_session` between
`backend/` and `deeptempo-core/`. In slop.py it found the `location_index` +
`entity_for` join written **seven** times, and `check::run` ↔ `check::audit`
sharing 8 distinctive callees while differing in one policy decision.

It also strictly extends `parallel-implementation`, which keys on *exact* callee-set
equality and therefore cannot see partial overlap at all.

---

## 6. Milestones

| M | Scope | Kill criterion |
| --- | --- | --- |
| **M0** | ~~Dataflow components from SCIP.~~ **Done, and failed.** Built tri-lingual; ~1 of 29 adjudicated findings was actionable. Not shipped. | Bar was "half worth acting on". Result ~3%. **Killed.** |
| **M1** | The benchmark, shipped *before* the tool. Self-labelling ground truth: mine history for a new function whose body matches lines deleted from an existing one — a mechanically detectable extract-method. Replay from pre-commit state, precision/recall **per signal**. | If MDL proposes 400 extractions per KLOC where humans made three, `L(call)` is wrong — and you know in an afternoon. |
| **M2** | Store + retrieval. Content-addressed defs by h₂, indexed by (type + effect signature), LSH sidecar over WL features. SCC-granular invalidation. The α-equivalence deny gate becomes real. | `check` on a changed file over ~1s makes the hook intolerable. |
| **M3** | Harness wiring: `Stop` hook for the sieve, auditor subagent, MCP tools (`search_by_type`, `search_by_shape`, `proposals`, `explain`, `suppress`). Mostly assembly — the surfaces exist. | — |
| **M4** | Anti-unification + MDL + co-change gate. **Read babble and Stitch first** (both POPL 2023); the top-down corpus-guided search in Stitch is the part that goes badly wrong when reinvented. | — |
| **M5** | Metrics, parallelizable with M4: co-change graph, conductance at plateau-stable γ, interface bit-width, modification ratio per axis. | — |

### 6.1 Where this goes next — three options

M0's kill criterion said "if half aren't things you'd act on, stop — nothing below
saves it." Taken literally that stops the whole programme. The diagnosis is
narrower than that, so the three live options, in order of my preference:

1. **Reformulate (1) around effects, not dataflow.** Require the two parts to
   touch *disjoint effect resources* as well as exchange no values. This uses the
   effect graph that already exists and directly answers the `refresh_custom_agents`
   class. Cheap — the plumbing is built. But it would have fired on **zero** of
   vigil's 1184 functions, so it also predicts a tool with nothing to say.
2. **Skip to signal (3) and prospective retrieval.** Neither depends on
   intra-procedural dataflow, so neither is touched by this result. Retrieval is
   the half that answers extend-vs-create *before* code exists, which was the
   original question; it needs no PDG and the IDF machinery exists.
3. **Stop the modularity engine.** Bank `stmt_spans`, the retained resolver and
   `split.rs` as substrate, and put the time into the CI gap, which is the
   recorded competitive risk.

Option 1's own prediction is the argument for 2 or 3: a signal calibrated until it
fires on nothing has been calibrated into agreement, not into correctness.

### M0, as built — four commits

**(a) `Analysis` retains the resolver.** `build_analysis`
(`crates/slop-analyze/src/check.rs:326`) already loads a `ScipResolver`, uses it
for the graph build and the file list, then drops it — so occurrence-level data is
built and thrown away on every run. Add `pub resolver: ScipResolver` to `Analysis`
(`check.rs:247`). Zero new work at runtime; it only extends a lifetime, and the
single-slot cache already holds the whole thing behind an `Arc`.

`detect::run_all` (`detect.rs:1039`) also needs the resolver to reach the new
detector — six call sites, two of them tests — but that thread lands in (c) with
the consumer, not here as an unused parameter.

**(b) `stmt_spans` on `FunctionFacts`.** The one thing SCIP does not encode:

```rust
/// Line span of each top-level statement in the body, in source order. SCIP
/// gives occurrences positions but no statement extents, and a multi-line
/// right-hand side must group with the target it defines.
pub stmt_spans: Vec<(u32, u32)>,
```

Recorded where each extractor already visits the body block — Python (`lib.rs`),
JS/TS (`js.rs`), Rust (`rust.rs`). Docstring/attribute leading runs are already
handled per language and stay excluded, same rule the spike uses.

**(c) `crates/slop-analyze/src/split.rs`.** Per function entity with a file and an
`enclosing_range`:

1. `resolver.occurrences_in(file)`, retained where the range falls inside
   `enclosing_range`.
2. Partition into internal (`SymbolKind::Local`, `Parameter`) and external
   (everything else — callees, imports, globals). External symbols are interface,
   not dataflow, and are reported rather than unioned.
3. Map each occurrence line to a statement index via `stmt_spans`.
4. `defs[i] & uses[j]` for `i < j` unions statements `i` and `j`. Parameters are
   never definitions, so two halves that merely both read one stay unlinked —
   that shared read *is* the interface width.
5. Components with ≥2 statements, when there are ≥2 of them, are the finding.
   Report each part's line range and the symbols it reads: its would-be signature.

Local symbol ids are **document-scoped** in SCIP, so the key is `(file, symbol)`.
`occurrences_in(file)` gives that grouping naturally; a repo-wide symbol map would
silently merge `local 1` across every file in the repo.

**(d) Detector registration + fixtures.** A `split-candidate` finding, wired into
`run_all`, with a fixture holding one genuinely-two-jobs function and one
tightly-coupled control that must **not** fire.

### Validating M0 against the spike

`bench/split_spike.py` stops being the implementation and becomes the **oracle**.
It computes the same relation from Python's own `ast`, with exact Store/Load
contexts and no index involved, so running both over vigil and diffing gives
something neither produces alone:

| Disagreement | What it means |
| --- | --- |
| spike splits, SCIP doesn't | usually SCIP's scope resolution killing a false edge the name match invented — SCIP is right |
| SCIP splits, spike doesn't | usually `self.x` or a closure capture the AST version never tracked — SCIP is right |
| either side splits a function the other has no record of | the `stmt_spans` → occurrence-line mapping is wrong, or the index is stale. **A bug, every time** |

The third column is the point. Without a second implementation, a mapping bug
looks exactly like a signal that doesn't fire, and the kill criterion below would
be adjudicating an empty list.

**Deferred indefinitely, and labelled as such**: e-graphs, equality saturation,
Knuth–Bendix, the transformation monoid. Equality saturation is where this
architecture converges if the signal is real — it exists precisely to destroy
phase ordering — but extraction under non-local cost is NP-hard, saturation times
out on real codebases, and **effects don't fit**: e-graphs work over pure terms,
and imperative code with mutation and ordering constraints does not slot in
without lifting to a functional IR with explicit effects.

**Cut order when behind**: TypeScript, then M5, then min-cut. **Never cut the
co-change gate or the benchmark** — the first is what makes proposals
trustworthy, the second is what tells you whether they are.

---

## 7. Risks

**The sieve is an amplifier.** "It doesn't matter if it's ugly as long as tests
pass" makes the suite the equivalence oracle. A weak suite admits code that
passes and is wrong; the sieve then canonicalises the wrongness and hoists it into
a shared module, propagating one bug to every call site. Two mitigations, and one
is free: **`untested-effect` already answers "does any test reach this code" — do
not hoist a fragment no test reaches.** Beyond that, an extraction must preserve
the conjunction of all instances' contracts (property and metamorphic tests
transfer where example tests don't), and extracted abstractions need tests
generated at extraction time or the shared module is untested and future edits
break call sites silently.

**The closed loop.** If the generator is prompted with retrieved canonical code
and the sieve canonicalises toward what the generator produces, the system
converges to a fixed point with no guarantee it is a good one. Keep a held-out
human-written corpus as an anchor and periodically check that canonical forms
still look like something a person would write.

**Contaminated ground truth.** The dogfood corpus is AI-developed, so "what a
human did" is largely what a model did. Precision against it partly measures
agreement with the behaviour being corrected. Signal (1) is unaffected — it is
provable, so it needs usefulness adjudication rather than a precision study — but
(2) and (3) calibrate thresholds against this history and inherit the problem.

**Same-file co-change is vacuous.** Two fragments in one file always co-change, so
the gate cannot discriminate exactly where duplication is most common.

**Naming kills adoption faster than false positives.** Mechanical slicing yields
`slice_1`, which is strictly worse for a reader than the ugly original. Hence D15.

**Some code should stay ugly.** Hot inner loops that are deliberately inlined and
repetitive need a recordable suppression, ideally machine-checkable ("the
benchmark shows inlining wins here") rather than a bare pragma.

---

## 8. Prior art — with corrections

- **babble** (POPL 2023) — library learning modulo theories; e-graphs plus
  anti-unification to extract reusable abstractions. Right starting point.
- **Stitch** (POPL 2023) — MDL scoring at scale, top-down corpus-guided, orders
  of magnitude faster than the ML approaches. Read before implementing §5(3).
- **EM-Assist** — real: LLM-proposed extract-method validated against IDE
  refactoring engines. Closest existing thing to the proposal step.
- **MirChecker** — ⚠️ **abstract interpretation over Rust MIR, not e-graphs.**
  The analysis is right for these purposes; the mechanism was misattributed.
- **"Leroy" as a Stitch wrapper for Python** — ⚠️ **could not be placed.** Likely
  a crossed wire with Xavier Leroy (verified compilers). The *capability* — lower
  Python ASTs to a Lisp-ish IR so Stitch can chew on them — is real and has been
  done; verify the name before relying on it.
- **Equality saturation** — Tate et al., POPL 2009. See §6 deferred.

---

## 9. Progress log

| Date | Entry |
| --- | --- |
| 2026-07-30 | `CONTEXT.md` created; four ambiguities resolved (modularity, modular-vs-extensible, sprawl, duplicate). |
| 2026-07-30 | ADR 0001 — deny only on α-equivalence. |
| 2026-07-30 | **h₂ built and committed.** Only *bound* names collapse; literals, callees, imports, globals and attribute names stay verbatim. Under-matches by design — for a deny gate that costs a missed refusal, never a wrong one. Five properties asserted. |
| 2026-07-30 | `bench/split_spike.py` written; two bugs found by re-reading before running (a `- params` subtraction that deleted real edges on parameter rebinding, and a `mutates_via_call` predicate true of nearly every call). **Not yet run.** |
| 2026-07-30 | **D20 — def-use from SCIP, not tree-sitter.** Checked before deciding: locals with definition occurrences are present in vigil's scip-python index and in slop.py's own rust-analyzer index, so the mechanism ports to all three languages rather than needing a binder table each. tree-sitter keeps the line→statement partition, which SCIP genuinely does not encode. |
| 2026-07-30 | Consequence of D20, recorded rather than glossed: **signal (1) leaves the write path.** It reads the index, and a function written thirty milliseconds ago has no occurrences at any budget. D19's placement table amended. |
| 2026-07-30 | **M0(a) written** — `Analysis` retains the `ScipResolver`. The resolver was already being built, used, and dropped on every run; occurrence data now survives the graph build. |
| 2026-07-30 | **M0(b) written** — `FunctionFacts.stmt_spans`, one shared `ts_state::stmt_spans` for the two tree-sitter frontends plus the ruff path, with a multi-line-RHS test per language. Expression-bodied arrows are one statement, not a partition of their subexpressions. |
| 2026-07-31 | **M0(c) written and M0 adjudicated. Signal (1) is dead.** 243 tests green. The oracle earned its place immediately: interleaved components were visible only in its line ranges, and reading three findings against real source identified guard chains as the dominant cause — confirmed at 31 of 60. Rust reproduced the failure with a fourth cause (recursive traversal). `split.rs` is retained but unwired; `run_all`'s signature was reverted rather than left carrying an unused parameter. |
| 2026-07-31 | D13 qualified: minimum cut is only as good as the edge set, and intra-procedural dataflow omits control transfer, shared mutable state and required ordering. The thesis holds at module scale, where the graph carries effects, and fails one scale down — losing exactly the scale-freeness that motivated it. Signal (2) inherits three of the four causes and should not be built as specified. |
| 2026-07-31 | Option 2 chosen: skip to prospective retrieval. **Built and it passes** — `retrieve.rs` + `slop suggest`. See §5. Three false-positive classes found by reading output and each removed by a principled fix, not a tuned threshold. Found real duplication in both dogfood repos, including cross-package duplication of secrets and database init in vigil. |
| 2026-09-22 | **Prospective retrieval wired.** `assess_write` exposes versioned, snapshot-bound policy and reuse evidence. `SessionStart`/indexing writes a deterministic callee sidecar; `PreToolUse` reads it without rebuilding the graph. Scores are integer parts-per-million and suggestions require at least `125_000`. This remains advisory. |
| | **NEXT: run the measurement contract in `DEFERRED_RESEARCH.md`; adjust the threshold only from labeled precision/recall evidence. Do not add a daemon, constrained decoder, e-graph, profiler service, or training loop before its gate passes.** |
