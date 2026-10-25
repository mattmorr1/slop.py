# Roadmap: sketch, bind, lower

Status: signed off 2026-10-25. P0 and P1 have PRDs in `docs/prd/`; P2 to P5
get theirs after gate G1, because G1 decides what they contain.

## Mission

> The agent sketches; slop binds the sketch to your codebase; the agent lowers it
> to production code.

A language model reads text well and computes poorly over a whole repository.
It cannot tell, from reading, that a new function is equivalent to one of 7,000
others, that a call reaches the network three frames down, or that the name it
just called does not exist. Those facts are cheap and exact to compute. slop
computes them and hands them back at the moment they change what gets written.
It is a feature map for the model, not a second model.

## Why the flow changes

The context engine asks *what will this edit need?* before any code exists. That
is intent-to-code retrieval, and the evidence says it is the wrong question:

- B4: 76% co-change recall at 8k, but about 10% precision. Nine of ten items
  shown were not needed.
- R8, two pre-registered runs: no ranker beat showing the target's own file.

Binding asks *what in this repository does this sketch already describe?* after
code exists. That is code-to-code matching over names the sketch calls, the
effects it performs and the shapes it builds, which is where exact methods work.

## The flow

1. **Sketch.** The agent (or a person) writes valid Python at the level of
   intent: real control flow, calls to names that may not exist, `...` bodies
   for parts not yet decided. No repository context needed beyond the target.
2. **Bind** (`slop bind <file|->`). For each unresolved call, stubbed function,
   raw effect and newly defined function, one binding:

   | binding | meaning | evidence |
   | --- | --- | --- |
   | `exists` | Call the existing entity; body excerpt and one real call site attached | E-sound equal (provable), or graded match with its features |
   | `channel` | Raw effect where the repo routes it through a sanctioned channel | policy plus effect signature (provable) |
   | `missing` | Called name resolves to nothing; nothing close exists | resolution (provable) |
   | `new` | Write it; suggested home is the directory with highest affinity | graded |

3. **Lower.** The agent rewrites the sketch into production code using the
   bindings.
4. **Reconcile** (`slop check` on the diff, as a Stop hook and in CI). It
   re-binds the lowered code and reports bindings that were ignored. It is the
   same engine run after the write, closing the loop.

Provable bindings may steer hard; graded ones are always phrased as candidates.
This keeps the deny-only-on-identity rule from ADR 0001 and ADR 0004.

## Decision: sketch format (decided: A)

| | A. Valid Python, unresolved names, `...` bodies (proposed) | B. Natural-language pseudocode in comments | C. Typed stubs only (signatures, Protocols) |
| --- | --- | --- | --- |
| Parse | ruff parser, existing | none | ruff parser |
| What bind sees | calls, effects, shapes, names | words only, which falls back to BM25, i.e. intent retrieval again | names and types, no behaviour |
| New syntax | none | none | none |
| Lowering distance | short | long | medium |

## What makes it tangible

1. `slop bind sketch.py` prints a binding table a person can act on in ten
   seconds; `--json` for agents. This is the demo.
2. A Claude Code plugin (P3): the `/slop:sketch` skill, MCP `bind`, and a
   Stop hook that will not let a turn end on an unaddressed provable binding.
3. LSP: hover on an unresolved name shows its binding; a code action inserts
   the call to the existing entity.
4. A GitHub Action that posts the reconcile table on a PR.
5. Fast mode: parse-only bindings (E-sound, `missing` within the file set,
   channel policy) without a SCIP index, so the first run needs no Node.
6. A scripted 60-second demo on a public repo, reproducible from the README.

## Phases

Each phase is one PRD. Gates are decided on numbers, not on whether it looks
promising.

### P0. Cut and consolidate (prerequisite for parallel agents)

- Remove: proxy, TUI, `fix`/repair/rename, tier3/LLM crate, context envelope
  path (`envelope`, `calibrate`, `relevance`, `context`, `compress`), the
  PostToolUse read compression, and the style rules (`naming-convention`, `slop-name`,
  `over-commenting`, `complexity-spike`, `circular-import`, `config-sprawl`).
  Keep `retrieve`, `search`, `prewrite`, `overlay` and `refresh`: bind needs
  them, and the last two keep MCP and LSP fresh after edits. The logistic fit
  goes too; P2 restores it from history if B5 shows fitted weights help. Git
  history keeps everything removed.
- Collapse 10 crates to 2: `slop-core` (parse, graph, resolve, analyze) and
  `slop` (CLI, MCP, LSP, hooks).
- Move the invariants cited as `D<n>` (35 files; D8, D9 most) from
  `ideation/EXECUTION_PLAN.md` into ADRs or `CONTEXT.md`, then delete
  `ideation/`.
- Archive superseded benches to `bench/archive/` with a summary that keeps the
  negative R8 result.
- Add `ci.yml`: test, clippy, hook latency on every push.
- Done when: `cargo test` and `clippy` pass, the CLI lists only
  `index check suggest gate setup launch uninstall doctor mcp lsp hook baseline init`
  (plus hidden `equiv` for B3 and `debug`),
  and the README matches.

### P1. B5, bind recall ($0, go/no-go for the whole roadmap)

Natural ground truth is thin: a probe over 6,975 commits of flask, httpx,
requests and click found 205 "function removed, callers redirected" candidates,
and the ones checked were redesigns, not caught duplicates. So B5 builds its
queries from model drafts instead:

- Targets: R8's 100 gutted functions (vigil, flask, httpx), whose gold is the
  original's repository-internal callees.
- Queries: isolated sketches of each target from local models on matt-pc
  (qwen3:8b, llama3.1:8b), given only the signature and docstring. Draft
  quality does not need to be high; the draft is the query.
- Arms at matched tokens: bind on the sketch; envelope on the target (pre-write,
  the current product); BM25 on the target; the target's file.
- Metrics: recall and precision of gold callees in the top 5 bindings; tokens
  shown; bind latency.
- Gate G1 (proposed, to pre-register): bind recall@5 beats the envelope by at
  least 10 points with a paired commit-cluster CI excluding 0, and precision@5
  is at least 30% (3x B4). Fail means stop and re-plan, not tune until it passes.
- Secondary set: the natural duplicates already confirmed in vigil (four
  `slugify`, `_env_bool` copied four times, the webhook helpers) as a precision
  check on real reinventions.
- Binder v0 is a Python reference over slop's outputs, as B4 was. It builds on
  `slop suggest`, which already ranks existing functions by callee overlap
  with code on stdin. P1 runs in parallel with P0 against a binary pinned at
  278c7fb, which still has the envelope arm.

### P2. `slop bind` in Rust

- Port the binder at B5 parity (same rankings, as B4's Rust matched the Python
  reference to the ppm).
- One schema-versioned, snapshot-bound JSON contract shared by CLI, MCP and LSP.
- Latency: p95 under 200 ms warm on vigil; fast mode under 2 s cold with no
  index.

### P3. Claude Code plugin and enforcement

Distributed as a plugin from this repository:
`/plugin marketplace add mattmorr1/slop`, then `/plugin install slop@slop`. It
replaces the Claude half of `slop setup`, which keeps Codex and editors.

```
.claude-plugin/marketplace.json
plugin/.claude-plugin/plugin.json   mcpServers.slop -> slop mcp
plugin/hooks/hooks.json             SessionStart, PreToolUse(Write|Edit|MultiEdit), Stop
plugin/skills/sketch/SKILL.md       sketch -> bind -> lower playbook
```

MCP tools shrink to five: `bind`, `reconcile`, `find_capability`,
`query_subgraph`, `acknowledge`. `assess_write` folds into `bind`;
`validate_change` becomes `reconcile`; the envelope tools go with P0. Fewer
tool descriptions also means fewer tokens on every request.

Enforcement is a ladder; each level is a setting in `slop.toml`:

| level | mechanism | blocks | default |
| --- | --- | --- | --- |
| L0 inform | MCP tools and skill | nothing; the agent may ignore them | on |
| L1 steer | PreToolUse `additionalContext` with bindings for the proposed write (today's hook) | nothing | on |
| L2 reconcile gate (default on, decided) | Stop hook returns `decision: block` when the turn's diff has unaddressed provable bindings (E-sound duplicate, `missing` name, channel bypass under policy) | finishing the turn, at most once (`stop_hook_active` passes) | on |
| L3 sketch-strict | per-session state machine `sketch -> bound -> lowering`; PreToolUse denies a production write whose sketch has not been bound | the write, with a reason that says to call `bind` | deferred until R9 measures it |
| L4 deny duplicate | PreToolUse `permissionDecision: deny` on an E-sound duplicate | the write | opt-in, provable only |

- Only provable bindings can block (ADR 0001, ADR 0004). Graded ones stay
  advisory at every level.
- `acknowledge(binding_id, reason)` records a decision in `.slop/acks.jsonl`,
  so a deliberate copy does not block again. The reason lands in the log for
  review.
- Hooks read the session-start sidecar and parse only changed files; they never
  build the graph. Stop-hook budget: p95 under 500 ms on vigil.
- Every hook decision is logged to `.slop/hooks.jsonl`; R9's arms are these
  levels.

Binary resolution, the one open plugin question:

| | A. `uvx slop@<pinned>` (proposed) | B. `slop` on PATH | C. binaries in the plugin |
| --- | --- | --- | --- |
| Install | none beyond `uv`; uses the P5 wheel | separate install step | none |
| Per-hook cost | resolution on every call unless SessionStart caches the path | none | none |
| Repo weight | none | none | 4 platform binaries per release |

Decided: A, with SessionStart resolving the path once and the hooks calling it
directly. That makes P5's wheel a dependency of P3.

LSP hover and code action for bindings ship separately; they serve people, not
the agent loop.

### P4. R9, Claude Code end to end (budget discussed before this starts)

- Pre-registered. Claude Code headless on historical tasks where the gold diff
  reused existing code. Arms are the plugin's enforcement levels: no plugin;
  L1; L2; L3 sketch-strict.
- Metrics: reinvented helpers per task, invented calls, tests passing, total
  tokens across passes.

### P5. Distribution

- Fast mode default when no index exists.
- PyPI binary wheel via maturin `bindings = "bin"` (`uvx slop`), alongside the
  existing GitHub release archives. crates.io deferred.
- GitHub Action, demo script, README rewritten around the flow.

## Order and parallelism

```
P0 ──────────────┐
P1 (pinned bin) ─┴─ G1 ── P2 ──┬── P3 (plugin) ── P4 (R9)
                               └── P5 (wheel) ──┘
```

P0 and P1 run in parallel. Nothing after G1 starts until G1 is decided. P3
needs P5's wheel for binary resolution; the plugin manifest and hooks can be
built against a local binary while the wheel lands.

## Non-goals

- New languages or detectors before G1.
- PR review in competition with hosted reviewers; CI is a delivery surface for
  reconcile, not a separate product.
