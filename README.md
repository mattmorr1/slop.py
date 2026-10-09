# slop

A static analyzer for Python, TypeScript/JavaScript and Rust that judges code
against the repository it lives in, plus a harness that feeds those judgments to
a coding agent while it writes.

It builds a graph of the repo from a [SCIP](https://github.com/sourcegraph/scip)
index, tags each function with the effects it performs (net, fs, db, env), and
flags reimplementations, bypassed infrastructure, dead code, misleading names
and tangled control flow.

Status: v0. The detectors are dogfooded on real repositories.

## Install

Needs a recent Rust toolchain. Indexing Python/TypeScript needs `npx`; Rust needs
`rust-analyzer`.

```sh
cargo install --path crates/slop     # or: cargo build --release
```

## Usage

Every command takes an optional repo path and defaults to the current directory.

```sh
slop index              # SCIP index for each detected language
slop check              # findings in the working-tree diff vs HEAD
slop check --all        # whole repo
slop check --json       # for editors and CI
slop gate               # exit non-zero on blocking findings, JSON verdict
slop dash               # TUI: browse, fix, dispatch to an agent, re-verify
```

```
WARNING (1)
  [complexity-spike] api.claude::chat  backend/api/claude.py:318
    `chat` is tangled: 31 control-flow branches nested 4 deep (cyclomatic 55)
    fix: Extract the deepest block (around line 428, nested 4 deep) into a
         named helper, or flatten it with early-return guard clauses

1 finding(s) — 0 blocking, 1 warning, 0 advisory
health: 84/100
```

A stale index is regenerated rather than trusted (`--reindex` forces it). Every
JSON result names the repository snapshot it was computed from.

## Rules

| Rule | Severity | Flags |
| --- | --- | --- |
| `infra-bypass` | Blocking | Raw net/fs/db/env access where the repo has a sanctioned channel for it |
| `circular-import` | Blocking | Import cycles |
| `effect-creep` | Blocking | A function that was pure at baseline now does I/O |
| `effect-layer-violation` | Warning | An effect a declared layer forbids |
| `duplicate-exact` | Warning | Identical bodies, ignoring comments and whitespace |
| `duplicate-equivalent` | Warning | Python functions provably equal up to renaming and sound rewrites ([ADR 0004](docs/adr/0004-e-equivalence-normalizer-with-egglog-as-oracle.md)) |
| `duplicate-structural` | Advisory | Same shape, different names; `--tier3` asks an LLM to confirm |
| `parallel-implementation` | Advisory | Same distinctive callees, no shared code |
| `complexity-spike` | Warning | Deep nesting or many independent branches |
| `purity-lie` | Warning | `compute_`/`parse_`/`is_` functions that do I/O |
| `dead-island` | Warning | Unreferenced, non-entry-point code (Advisory for methods) |
| `untested-effect` | Advisory | Branching I/O no test reaches, in a repo that usually tests it |
| `config-sprawl` | Advisory | One env var read directly in four or more modules |
| `naming-convention` | Advisory | Deviates from the repo's dominant case style |
| `slop-name` | Advisory | `_v2`, `helper_`, `temp_` |
| `over-commenting` | Advisory | Comments that restate the code |

`infra-bypass` needs a `slop.toml` policy; `slop init` proposes one from the
repo's dominant patterns. `slop baseline` grandfathers current findings and
records effect signatures for `effect-creep`.

## Fixing

`slop fix` is a dry run; `--write` applies. It renames camelCase free functions
across every SCIP-resolved reference. Opt-in flags also remove restating
comments, dead functions and trivial wrappers. Each repair checks
preconditions, reparses, reindexes and confirms the finding is gone, or rolls
back. Everything else ships as `fix_guidance` text for an agent.

## Agent harness

```sh
slop setup --ai claude --editor cursor   # or --ai codex
slop launch
slop uninstall                           # removes only what setup wrote
```

- **MCP** (`slop mcp`): `find_capability`, `assess_write`, `validate_change`,
  `get_context_envelope`, `expand_entity`, `query_subgraph`.
- **Hooks**: write-time duplicate checks, and read-path compression that keeps
  code near the edit in full and skeletonizes the rest.
- **Proxy** (`slop proxy`): an `ANTHROPIC_BASE_URL` reverse proxy with request,
  concurrency and timeout limits.

Details: [docs/harness.md](docs/harness.md).

## Context engine

`get_context_envelope` returns, for an entity about to be edited, the source an
agent most likely needs under a token budget.

- **Candidates**: undirected BFS over calls, imports and contains, up to 4 hops.
  Nodes with fan-in above 50 are reached but not expanded. Plus every entity in
  the target's directory.
- **Scoring**: logistic regression over 12 features (hop distance, same file,
  same directory, line gap, same container, shared effect, is-class, BM25
  against the target) giving P(co-change). Fit on commit history; each item
  returns its per-feature logit contributions.
- **Packing**: hop 1 in full, skeletons (effects, signature, docstring) beyond;
  p < 0.01 dropped outside hop 1. Lazy greedy on gain per token, with provably
  equivalent functions counted once.
- **Determinism**: probabilities in parts per million, integer ratio
  comparison, entity-id tie-break. Output is bound to a content-hashed
  snapshot; `expand_entity` reads from the same one.
- **Freshness**: an edited file is reparsed at once (p95 408 ms) and reindexed
  in the background.
- **Calibration**: `slop calibrate` refits on the repo's own history and writes
  `.slop/relevance.json` only if it does at least as well on the newest commits.

B4 benchmark, out of sample: 76.0% co-change recall at 8k tokens, +6.3 points
over BM25 and +11.5 over Aider's file-plus-map; p95 9 ms on Sentry. Design in
[ADR 0005](docs/adr/0005-calibrated-relevance-for-context-selection.md) and
[ADR 0006](docs/adr/0006-reparse-overlay-and-background-refresh.md); numbers in
[bench/README.md](bench/README.md).

## Editors

`slop lsp` publishes findings as diagnostics on open and save (Blocking = Error,
Warning = Warning, Advisory = Information). Any LSP client works;
[editors/vscode](editors/vscode) has a VS Code/Cursor launcher.

## Limitations

- Resolution is as good as the SCIP index: dynamic dispatch, `getattr` and duck
  typing can be missed.
- Read-path compression cuts tokens about 61% on dogfood repos. Whether output
  quality holds is not established; the first end-to-end test (R8) found the
  envelope no better than showing the target's file.
- `fix` renames are index-verified, not proven behaviour-preserving. Review the
  dry run.
- The VS Code launcher has no tests; the LSP server does.

## Development

```sh
cargo test && cargo clippy
```

Decisions are in [docs/adr](docs/adr); the invariants the code cites as `D<n>`
are in `ideation/EXECUTION_PLAN.md`.
