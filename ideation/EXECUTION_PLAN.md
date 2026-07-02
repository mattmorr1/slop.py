# `slop` — Execution Plan

> A codebase-relative, effect-graph-driven analyzer that detects AI slop, steers agents away from it, and compresses the context those agents need — Python-first, Rust-built.

This document is the synthesis of `idea.md` and `gemini-idea.md` after a full design interview. It records the locked decisions, the architecture, the milestone sequence, and the risks.

---

## 1. Thesis & North Star

Generic linters (ruff, pylint) judge a file **in isolation against universal rules** — types, unused vars, complexity. That space is owned and commoditized.

`slop`'s differentiator is **codebase-relative analysis**: judging new code **against the whole graph of what the codebase already is and already does.**

- *"You reimplemented* `parse_date` *— the codebase already has* `utils.dates.parse`*."* (redundancy vs. existing capability)
- *"You wrote an ad-hoc HTTP retry loop — this codebase routes all network I/O through* `core.http.Client`*."* (bypassing existing infrastructure)
- *"This function is named* `helper_process_data_v2` *— nothing else here is named like that."* (conformance to house style)

Slop is defined **relative to the surrounding codebase**, not absolutely. That is the entire point.

**The engine that makes this possible is a static effect graph.** Per the literature scan, repository graphs, skeletal compression (Aider), and static-analysis-guided generation (Monitor-Guided Decoding) all exist. The genuine white space — our defensible novelty — is **static effect inference used as an AI-code-quality guardrail and context signal.** We build the graph and skeletons by *borrowing* proven work (SCIP, Aider's budgeted packing, RepoGraph's edge taxonomy); we innovate on the **effect layer**.

---



## 2. Locked Decisions (Decision Log)


| #       | Decision                                                                                                                                                                                                                                                                                                                                                                                                                           | Rationale                                                                                                                                                   |
| ------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **D1**  | **v1 = CLI linter.** The graph + analysis engine is the reusable core that all later products consume; the CLI is the cheapest way to exercise it.                                                                                                                                                                                                                                                                                 | Standalone value, zero integration risk, forces the shared core.                                                                                            |
| **D2**  | **Flagship = the effect graph.** Headline checks: infra-bypass, effect-layer violations, purity-lies. Effect-bucketed redundancy is feature #2.                                                                                                                                                                                                                                                                                    | Effect-as-guardrail is the true novelty; redundancy is the demo everyone understands.                                                                       |
| **D3**  | **Effect inference is coarse & conservative.** ~8–10 effect categories, hand-curated seed table, `Unknown` as the top of the lattice.                                                                                                                                                                                                                                                                                              | Precise effect inference in dynamic Python is research-grade; coarse effects are tractable and still sufficient for every check.                            |
| **D4**  | **Duplication: Tier 1 (exact body-hash) + Tier 2 (structural near-dup) are deterministic.** Tier 3 (semantic redundancy) uses effect-bucket prefilter + fuzzy/LLM confirm and is **advisory, never blocking.**                                                                                                                                                                                                                     | Deterministic tiers have near-zero false positives; fuzzy Tier 3 would destroy trust if it blocked.                                                         |
| **D5**  | **Name resolution lives behind an adapter.** v1 backs it with `scip-python` (pyright-grade, ingested as a batch index). Migrate to native Rust (`ty`/Ruff semantic model) later; contributing upstream is the migration path, not a v1 blocker.                                                                                                                                                                                    | Reuse mature resolution; keep the artifact decoupled so the eventual clean daemon is a swap, not a rewrite.                                                 |
| **D6**  | **The graph is one cyclic, multi-edge-kind directed graph** (`Contains`, `Calls`, `Imports`, `Inherits`, `HasEffect`), callable-level nodes, effects as edges on the *same* graph. Acyclicity is **checked per-edge-kind** (e.g. `Imports` should be a DAG; a cycle is the *finding*), never a global invariant.                                                                                                                   | Gemini's "H-DAG" is mislabeled — call/import graphs have cycles, and we *want* to detect them (Tarjan SCC).                                                 |
| **D7**  | **Primary mode = diff-relative.** The whole repo is the *baseline/model*; the diff is what gets *judged*. `slop audit` (whole-repo) is secondary and builds the baseline.                                                                                                                                                                                                                                                          | AI slop is a *delta* phenomenon; solves the adoption problem (no 4000-finding storm on existing code); makes creep/redundancy/bypass correctly-defined.     |
| **D8**  | **Sanctioned channels are dominant-pattern-inferred → written to an editable** `slop.toml` **→ user-confirmed.** Empty policy ⇒ infra-bypass checks stay silent (report-only).                                                                                                                                                                                                                                                     | No cold-start false-positive storm; no config-writing burden; escape hatch preserved.                                                                       |
| **D9**  | `slop` **is never its own coding agent.** It is: (a) a deterministic auto-fixer for safe mechanical issues, (b) a structured findings engine with machine-readable fix-guidance, (c) an MCP tool surface + validation gate that an *existing* agent (Claude Code, Codex) drives.                                                                                                                                                   | Building an agent = reinventing Claude Code = the exact sin we detect. "Full CLI coding agent" is a **parked non-goal.**                                    |
| **D10** | **Interception layer matrix** (v2+): compression → `PostToolUse` hook (`updatedToolOutput`, read-path only); agent-agnostic compression → tool-call shim; steering/observability/gating → `ANTHROPIC_BASE_URL` proxy. **Never cert-MITM.** Compression is **read-path only, zoned by graph distance** (full fidelity in the edit zone, skeletons beyond) — this resolves the edit-invertibility problem.                           | Hooks intercept *before* context pollution (preserves KV cache, no state divergence); the proxy is too late for compression but right for uniform steering. |
| **D11** | **Context envelope: effect-typed relevance scorer + budgeted greedy packing.** PageRank **rejected** — it measures global topological centrality, which is the wrong signal for a specific edit. Scoring = effect-signature relevance + type-contract adjacency + convention exemplars + call/containment distance (one term). Skeleton = `(type sig + effect sig + docstring)` contract + deterministic whitespace/comment strip. | This is where we beat Aider: a semantic effect-typed model of the code, denser and smarter than a signature-only repo map.                                  |
| **D12** | **Severity = confidence × impact, collapsed onto the detector taxonomy.** Confidence is intrinsic to the detector (deterministic ⇒ can block; fuzzy ⇒ can only advise). Health = severity-weighted slop-density; **diff-delta is the headline number** (`82 → 79`), absolute repo score in audit mode. Baseline file + reasoned inline suppression (`# slop: allow <rule> — <reason>`). No tamagotchi.                             | Severity isn't a new axis to invent; it falls out of the detector confidence we already defined.                                                            |


---



## 3. Architecture



### 3.1 Layered stack

```
  ┌─────────────────────────────────────────────────────────────┐
  │  Delivery                                                     │
  │   v1: CLI (`slop check` diff-mode, `slop audit` whole-repo)   │
  │   v2: MCP tools + PostToolUse read-remap hook (the "harness") │
  │   v3: validation gate + worktree fix-loop + base-URL proxy    │
  ├─────────────────────────────────────────────────────────────┤
  │  Analysis (the product)                                       │
  │   Detectors  →  Severity/Health  →  Findings + fix-guidance   │
  │   Context envelope (effect-typed relevance + budgeted pack)   │
  ├─────────────────────────────────────────────────────────────┤
  │  Effect graph                                                 │
  │   Effect inference (coarse lattice, seed table, transitive)   │
  │   One cyclic multi-edge-kind graph (Contains/Calls/Imports/   │
  │   Inherits/HasEffect), callable-level nodes                   │
  ├─────────────────────────────────────────────────────────────┤
  │  Foundation                                                   │
  │   Resolution adapter  ──backed by──▶  scip-python (v1)        │
  │                                       ty / Ruff model (later) │
  │   Parser: Ruff AST crates (revisit tree-sitter iff daemon)    │
  └─────────────────────────────────────────────────────────────┘
```



### 3.2 Graph node (evolved from Gemini's `CodeEntity`)

```rust
pub enum NodeType { Module, Class, Function, Import, EffectSource }

pub struct CodeEntity {
    pub id: String,                    // "billing.gateways::StripeGateway::charge_customer"
    pub entity_type: NodeType,
    pub name: String,
    pub signature: String,             // full type-annotated signature
    pub docstring: Option<String>,
    pub source_range: (usize, usize),
    pub body_hash: String,             // Blake3 — Tier-1 dup
    pub effect_signature: EffectSet,   // ADDED — the differentiator
    // cyclomatic_complexity REMOVED as a stored field — compute on demand
}
```

Edge kinds: `Contains`, `Calls` (cyclic), `Imports` (cyclic — SCC target), `Inherits`, `HasEffect` (→ `EffectSource` nodes).

### 3.3 Effect lattice (D3 — coarse & conservative)

`Pure`, `Net`, `FS(read|write)`, `DB`, `Env`, `Throws(T)`, `Time/Random` (nondeterminism), `State(mutate)`, `Concurrency`, `Unknown` (top).

- **Seed table:** hand-curated map of primitive effect sources (`requests.`*/`socket.*` → `Net`, `open().write` → `FS(write)`, `os.environ` → `Env`, …) for stdlib + top-~20 libs.
- **Propagation:** transitive up the `Calls` graph via fixpoint over SCCs. Unresolved call ⇒ `Unknown`, handled gracefully (never crashes, caps recall not precision).



### 3.4 Detector taxonomy → severity (D2, D4, D12)


| Detector                                              | Confidence    | Max severity | Notes                                 |
| ----------------------------------------------------- | ------------- | ------------ | ------------------------------------- |
| Infra-bypass (confirmed sanctioned channel)           | deterministic | **Blocking** | flagship                              |
| New circular import                                   | deterministic | **Blocking** | Tarjan SCC on `Imports`               |
| Effect-creep on declared-pure fn                      | deterministic | **Blocking** | delta vs. baseline                    |
| Effect-layer violation (e.g. `DB` in presentation)    | deterministic | **Warning**  | generalizes Gemini's hardcoded rule   |
| Purity-lie (name implies pure, effects say otherwise) | deterministic | **Warning**  |                                       |
| Cyclomatic complexity spike                           | deterministic | **Warning**  | compute on demand                     |
| Dead island (in-degree 0, not an API entry)           | deterministic | **Warning**  |                                       |
| Tier-1 exact duplicate                                | deterministic | **Warning**  | Blake3                                |
| Tier-2 structural near-duplicate                      | deterministic | **Warning**  | normalized-AST fingerprint            |
| Over-commenting / comment-restates-code               | deterministic | **Advisory** | + auto-fixable                        |
| Tier-3 semantic-redundancy candidate                  | fuzzy/LLM     | **Advisory** | effect-bucket prefilter, never blocks |
| Naming-convention deviation                           | fuzzy         | **Advisory** | vs. codebase population               |


**Rule:** fuzzy detector ⇒ structurally incapable of `Blocking`.

### 3.5 The two jobs of one graph (v2)

- **Prevention (pre-hoc):** inject `additionalContext` when the agent reads — *"this codebase routes* `Net` *through* `core.http.Client`*; don't use* `urllib`*."*
- **Detection (post-hoc):** the validation gate catches what slipped through.

---



## 4. Milestones


| M          | Name                 | Scope                                                                                                                                                                                              | Exit criterion                                                                                                |
| ---------- | -------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------- |
| **M0**     | Spike                | `scip-python` ingest + graph build + parse on a toy repo. Mostly throwaway.                                                                                                                        | Resolution adapter returns correct defs for a 3-file repo.                                                    |
| **M1**     | **Walking skeleton** | Single vertical thread: parse → resolve → graph → infer **one effect (**`Net`**)** → detect **one finding (infra-bypass)** → diff-mode CLI output with severity.                                   | Fires correctly on a slopped repo; stays silent on clean code. Every architectural layer exists.              |
| **M2**     | **v1 (shippable)**   | Full effect lattice + seed table (top libs); full **deterministic** detector set (§3.4 rows 1–10); health score + baseline + suppression; `slop.toml` policy inference.                            | Runs on all 3 dogfood repos with a defensible finding set and manageable false-positive rate.                 |
| **M3**     | v1.1                 | Tier-3 effect-bucketed redundancy + LLM advisory + naming-convention detection. Still CLI.                                                                                                         | Tier-3 candidates surface real redundancies without blocking.                                                 |
| **M4**     | **v2 — the harness** | Effect-typed envelope scorer + budgeted packing; MCP additive tools (`get_context_envelope`, `query_subgraph`, `validate_change`); `PostToolUse` read-remap (zoned); `additionalContext` steering. | Measurably fewer tokens per task *and* fewer slop findings introduced, validated via the observability proxy. |
| **M5**     | v3 — the gate        | Validation gate + agent-driven fix-loop in a worktree; `ANTHROPIC_BASE_URL` proxy for agent-agnostic steering/observability.                                                                       | Gate rejects a bad edit and loops correct feedback to the agent.                                              |
| **Parked** | Non-goals            | VS Code extension, Claude skill, other languages (= more seed tables + SCIP indexers), own coding agent.                                                                                           | Revisit only after v1–v2 prove the thesis.                                                                    |


**Hard ordering constraint:** finish M2/M3 (detection, CLI-only) **before** M4 (harness). The harness's value is unprovable until the effect graph is trustworthy — otherwise you tune ranking against a graph that's still 30% wrong.

---



## 5. Dogfooding Corpus

Three AI-developed repos with deliberately different slop profiles (avoids overfitting detectors to one style):


| Repo              | Profile                                                       | Exercises                                                            |
| ----------------- | ------------------------------------------------------------- | -------------------------------------------------------------------- |
| `vigil`           | Expansive SOC daemon; heavy vibe-coding + MCP implementations | infra-bypass, effect-layer violations, MCP/daemon sprawl, redundancy |
| `stress-analysis` | UX + health-analysis app                                      | naming conventions, dead islands, complexity, over-commenting        |
| `geoguessrbot`    | ML-oriented                                                   | effect-creep (I/O in "pure" compute), duplication, ad-hoc infra      |


Baseline via `slop audit` on each; then validate `slop check` diff-mode against representative commits.

---



## 6. Risks & Validation Timing


| Risk                                           | Severity | Validate at   | Mitigation                                                                         |
| ---------------------------------------------- | -------- | ------------- | ---------------------------------------------------------------------------------- |
| SCIP ingest fidelity / resolution blind spots  | High     | M0–M1         | Adapter boundary lets us swap resolvers; coarse effects tolerate `Unknown`.        |
| Effect inference imprecision on dynamic Python | High     | M1            | Coarse lattice + conservative `Unknown`; advisory-not-blocking for low-confidence. |
| Infra-bypass false-positive rate               | High     | M1–M2 dogfood | Dominant-pattern inference + user-editable policy + report-only when policy empty. |
| Tier-3 redundancy noise                        | Medium   | M3            | Effect-bucket prefilter narrows candidates; strictly advisory.                     |
| Compression doesn't improve agent outcomes     | Medium   | M4            | Observability proxy measures tokens + introduced-slop before/after.                |
| Ruff AST crate API churn                       | Low      | ongoing       | Pin versions; parser is swappable.                                                 |


---



## 7. Positioning vs. Prior Art

**Borrow, don't reinvent** (the tool's own thesis, applied to itself):

- **Resolution:** `scip-python` (Sourcegraph/pyright).
- **Skeletal compression + budgeted packing machinery:** Aider's repo map (but **swap PageRank for the effect-typed scorer** — D11).
- **Graph edge taxonomy:** RepoHyper / RepoGraph.
- **Symbol-valid generation guardrail (later):** Monitor-Guided Decoding.
- **Hierarchical localize→repair pattern:** Agentless.

**Innovate here:** static effect inference as an AI-code-quality guardrail and context-selection signal — confirmed underexplored.

### Key references

- Aider repo map — Gauthier, 2023 (production reference for graph-ranked skeletons).
- RepoGraph — Ouyang et al., ICLR 2025 (arXiv 2410.14684).
- RepoHyper — Phan et al., FORGE 2025 (arXiv 2403.06095) — semantic graph, *not* a hypergraph.
- CodePlan — Bairi et al., FSE 2024 (arXiv 2309.12499).
- RepoCoder — Zhang et al., EMNLP 2023 (arXiv 2303.12570).
- Monitor-Guided Decoding — Agrawal et al., NeurIPS 2023.
- LongCodeZip / CodeCompressor — 2025 (closest published AST-skeleton compression; sharpen differentiation against it).
- Agentless — Xia et al., FSE 2025 (arXiv 2407.01489).
- Semantic (Type-4) clone detection — reusable for duplication detectors.

long term: test on gemini flash 3.5 swe-bench lite.