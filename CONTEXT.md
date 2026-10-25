# slop

Codebase-relative analysis of AI-generated code: judging new code against the graph of what a codebase already is and already does, rather than against absolute rules.

## Language

### What is being judged

**Slop**:
Code that is wrong *relative to the codebase it lives in* — redundant with something that exists, bypassing infrastructure everyone else routes through, or unreachable.
_Avoid_: bad code, low-quality code, technical debt (all absolute judgements; slop is always relative)

**Finding**:
One located, rule-attributed statement that a specific entity exhibits slop, carrying guidance precise enough to act on.
_Avoid_: error, warning, issue, violation (those name severities or trackers, not the unit)

**Entity**:
A callable or container the graph holds a node for — module, class, or function.

### Grades of sameness

Distinct claims, ordered by strength. Conflating them is how a duplicate
detector either misses renames or blocks legitimate code.

**Textually identical**:
Two bodies that differ only in comments and whitespace. Defeated by any rename.

**α-equivalent**:
Two bodies that differ only in the *names* they use — literals, structure and
call targets all preserved. This is an identity rather than a judgement, which is
what qualifies it to block a write (see ADR 0001).
_Avoid_: structurally identical (that name is already taken by the weaker claim below)

**E-equivalent (sound)**:
α-equivalent modulo a fixed set of rewrite laws that hold for *every* value in
the language: a ternary and its two-branch `if`, a negated test with swapped
branches, unreachable code after an exit, a single-use temporary before a
return. Still an identity, so still strong enough to block a write (ADR 0004).

**E-equivalent (graded)**:
Equivalent modulo laws that hold only for well-behaved types: commutation,
comparison flips, De Morgan, `x += y` as `x = x + y`. Python's `str +` does not
commute and `list.__iadd__` aliases, so this is a judgement, never an identity.
_Avoid_: semantically equivalent (no tier here decides that)

**Same shape**:
Two bodies with the same control flow, where names *and literals* are both
disregarded — so a differing constant does not distinguish them. A candidate for
review, never a proof of duplication.

### Signals

**Provable signal**:
One whose output is a fact about the code — an identity, or the absence of a
dataflow path. It can be acted on without a precision study, and only a provable
signal may deny a write.

**Graded signal**:
One with a free parameter — a threshold, a ratio, a cost estimate. Always
advisory, and requires calibration against evidence outside the code itself.

### The graph

**Effect**:
One of ~10 coarse categories of what code *does* to the world (net, fs_read, fs_write, db, env, throws, nondeterminism, state, concurrency, unknown), deliberately imprecise with `unknown` as the top of the lattice.
_Avoid_: side effect, IO (too narrow — `nondeterminism` is neither)

**Effect signature**:
The set of effects an entity performs, including those inherited transitively through its callees.

**Sanctioned channel**:
The entity a codebase has evidently chosen to route one effect through, inferred from dominant pattern and confirmed in `slop.toml`. Code elsewhere calls it rather than acquiring the effect directly.
_Avoid_: wrapper, abstraction, service layer

**Capability**:
A callable the codebase already provides, offered to an agent as an alternative to writing a new one.

### The harness

**Repository snapshot**:
An immutable, content-identified generation of indexed source, graph, effect
signatures, policy, baseline, and parsed facts. Every finding and context
artifact names the repository snapshot it came from.

**Judgment**:
The deterministic projection that turns one repository snapshot into ordered
findings after suppression, submodule exclusion, baseline classification, and
scope selection. CLI, MCP, hooks, gate, and baseline share it.

**Context artifact**:
A deterministic, provenance-bearing selection of full source and contract
skeletons for one repository snapshot, locus, edit zone, and token budget.
A document edited since indexing is **reparsed** (fresh line ranges from the
parser, the last index's edges) and named as such in the artifact; one the
parser cannot anchor degrades explicitly to verbatim source.

**Prewrite assessment**:
A snapshot-bound projection of proposed source into direct policy findings and
graded reuse suggestions. It may steer a write; suggestions never deny one.

**Repair plan**:
A schema-versioned, snapshot-bound set of explicit source transformations with
safety classes and source preconditions. It remains pending until replacement
judgment succeeds, then commits or restores its journaled originals.

**World model**:
The codebase facts an agent is given *before* it designs anything — channels, layer rules, the capability index, the config surface. Distinct from a finding, which arrives after a decision was already made.

**Edit zone**:
The files an agent has recently written, plus everything within a small graph distance of them. Read full-fidelity; everything beyond is skeletonized.

**Steering**:
Facts injected into an agent's context to change what it writes. Never a refusal — slop reports and informs, it does not block or edit.

### Deciding where code goes

**Interface width**:
The number of values that must cross a proposed boundary between two pieces of code — the cut size of the split.
_Avoid_: coupling, API surface (both vaguer, and neither is a number)

**Split candidate**:
A function whose statements form two or more components with no dataflow between them — two functions concatenated, where the boundary is derivable rather than a judgement call.

**Axis of change**:
The direction a codebase actually grows, measured from history: adding cases versus adding operations. "Extensible" is meaningless until the axis is named.

## Relationships

- A **Finding** names exactly one **Entity** and one rule
- An **Entity** has one **Effect signature**, which is the union of its own **Effects** and its callees'
- A **Sanctioned channel** is an **Entity** that acquires an **Effect** on everyone else's behalf
- A **Capability** is an **Entity** proposed for reuse; the **World model** is a budgeted selection of them
- A **Judgment** and a **Context artifact** name exactly one **Repository snapshot**
- A **Prewrite assessment** names exactly one **Repository snapshot** and may name zero or more existing **Capabilities**
- A **Repair plan** names exactly one **Repository snapshot** and settles only after a replacement **Judgment**
- A **Split candidate** is an **Entity** whose **Interface width** at some internal boundary is small enough that splitting costs less than it saves

## Example dialogue

> **Dev:** "This function reads `DATABASE_URL` straight from the environment — is that a **Finding**?"
> **Domain expert:** "Only if the codebase has a **Sanctioned channel** for `env`. With no channel there's nothing to deviate from, so slop stays quiet."
> **Dev:** "And if I want to add retry logic — new function, or extend the existing one?"
> **Domain expert:** "Ask what the new code's **Interface width** would be. If it shares dataflow with the existing body, extending is cheaper. If the two halves wouldn't exchange any values, you have a **Split candidate** and it was two functions all along."

## Flagged ambiguities

- **"Modularity"** was used to mean three different things at once: cohesion, adaptability, and net code volume. Qualified by measurement: graph partitioning remains useful at module scale, where call and effect edges carry architectural evidence. It failed at statement scale because dataflow omitted control transfer, shared mutable state, required ordering, and recursive traversal. Orbit does not claim one scale-free minimum-cut computation.
- **"Modular" vs "extensible"** are not the same claim. Modularity is scale-free; extensibility is directional and undefined until an **Axis of change** is named.
- **"Sprawl"** was used for both "too many functions" and "too much code". Resolved: only unit count weighted against **Interface width** is measurable; total volume on its own is not a defect.
- **"Duplicate"** was used for all three **grades of sameness** at once. Resolved: they are separate claims, and the distinction is load-bearing — only **α-equivalence** is strong enough to deny a write, and the codebase's two existing hashes bracket it without hitting it.
- **"Hard gate"** was used to mean both "refuse the write" and "make the wrong thing unrepresentable". Resolved: slop only does the former, and only on a **provable signal**. Making a thing unrepresentable requires authoring the shape the model generates into, which is a different product.

## Invariants

Locked decisions the code cites by id. They come from the original execution plan's decision log, trimmed to what survives P0; ids are stable and never reused.

- **D2: the effect graph is the flagship.** Headline checks are infra-bypass, effect-layer violation, effect-creep and purity-lie; effect-bucketed redundancy comes second. Effect-as-guardrail is the novelty; redundancy is the demo everyone understands.
- **D3: effect inference is coarse and conservative.** About 8 to 10 effect categories, a hand-curated seed table, and `Unknown` as the top of the lattice. Precise effect inference in dynamic languages is research-grade; coarse effects are tractable and still sufficient for every check.
- **D4: duplication tiers 1 and 2 are deterministic.** Tier 1 is an exact body hash, tier 2 a structural near-duplicate hash, and their output order is deterministic. Semantic redundancy is advisory, never blocking: a fuzzy signal that blocked would destroy trust.
- **D5: name resolution lives behind an adapter.** It is backed by batch SCIP indexes (scip-python, scip-typescript, rust-analyzer). Reuse mature resolution and keep the artifact decoupled, so a native resolver later is a swap, not a rewrite.
- **D6: the graph is one cyclic, multi-edge-kind directed graph.** Edges are `Contains`, `Calls`, `Imports`, `Inherits` and `HasEffect`, with callable-level nodes and effects as edges on the same graph. Acyclicity is a per-edge-kind question, never a global invariant: call and import graphs have cycles.
- **D7: the primary mode is diff-relative.** The whole repo is the baseline and model; the diff is what gets judged. `slop check --all` is secondary and builds the baseline. Slop is a delta phenomenon, and this avoids a 4000-finding storm on existing code.
- **D8: sanctioned channels are inferred from the dominant pattern, written to an editable `slop.toml`, and confirmed by the user.** An empty policy keeps infra-bypass and every other policy-relative check silent. No cold-start false-positive storm and no config-writing burden.
- **D9: slop is never its own coding agent.** It is a findings engine with machine-readable fix guidance, plus an MCP tool surface and validation gate that an existing agent (Claude Code) drives. Building an agent would reinvent the host, which is the exact sin slop detects.
- **D12: severity is confidence times impact, collapsed onto the detector taxonomy.** Confidence is intrinsic to the detector: deterministic can block, fuzzy can only advise. Health is severity-weighted slop density; the diff delta is the headline number. A baseline file and reasoned inline suppression (`# slop: allow <rule> — <reason>`) handle existing code.
- **D20: def-use comes from SCIP, not from a tree-sitter walk.** Occurrences inside a function's enclosing range, filtered to local and parameter symbols, give scope-exact definitions and uses. tree-sitter keeps the one job it is right for: partitioning body lines into top-level statements, which SCIP does not encode.
