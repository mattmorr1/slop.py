# slop

A **codebase-relative AI-slop analyzer** for Python, plus an agent harness that
steers a coding agent away from slop as it writes.

Most linters judge a file against absolute rules. `slop` judges code against
*the codebase it lives in*: it builds an effect-typed graph of the whole repo
and flags the things that make AI-generated code rot — reimplementing something
that already exists, bypassing the infrastructure everyone else routes through,
hallucinated dead code, functions whose names lie about what they do, and
tangled control flow — then either fixes them mechanically or hands an agent
precise, located guidance.

> Status: **v0**, Python-only, and built on a `scip-python` index. The
> detectors and harness are dogfooded on real repos (see below); the
> token/quality *value* of the read-path harness is measured but not yet
> rigorously proven. See [Limitations](#limitations).

## Install

Requires a recent Rust toolchain and `npx` (for the Python indexer).

```sh
cargo install --path crates/slop     # installs the `slop` binary
# or, without installing:
cargo build --release                # -> target/release/slop
```

## Quickstart

`slop` reads a [SCIP](https://github.com/sourcegraph/scip) index of your repo.
Generate one, then check:

```sh
slop index .            # runs scip-python and verifies the index isn't empty
slop check .            # judges the working-tree diff vs HEAD
slop check . --all      # audits the whole repo
```

(`slop index` wraps `npx @sourcegraph/scip-python`; run that directly if you
prefer. A broken/empty index fails loudly rather than silently passing.)

```
WARNING [complexity-spike] api.claude::chat (backend/api/claude.py:318)
  `chat` is tangled: 31 control-flow branches nested 4 deep (cyclomatic 55)
  fix: Extract the deepest block (around line 428, nested 4 deep) into a named
       helper, or flatten it with early-return guard clauses

12 finding(s), 1 blocking
health: 84/100
```

Every run ends with a **health score** (0–100). `slop check` exits non-zero
when anything is *Blocking*.

## What it flags

| Rule | Severity | What it catches |
| --- | --- | --- |
| `infra-bypass` | Blocking | Acquiring a raw effect (net/fs/db/env) directly when the codebase routes that effect through a sanctioned channel |
| `circular-import` | Blocking | Import cycles (Tarjan SCC over the import graph) |
| `effect-layer-violation` | Warning | An entity in a declared layer directly does an effect that layer forbids (e.g. DB/net in a `pure-utils` or presentation layer) |
| `effect-creep` | Blocking | A function that was **pure** at `slop baseline` time now performs I/O — a purity regression (delta vs baseline) |
| `duplicate-exact` | Warning | Functions with identical bodies (modulo comments/whitespace) |
| `duplicate-structural` | Advisory → Warning | Same control-flow shape, renamed vars/literals — a *candidate*; `--tier3` promotes ones an LLM judge confirms |
| `complexity-spike` | Warning | Genuinely tangled functions — deep nesting or many independent branches, not just a fat boolean guard |
| `purity-lie` | Warning | A `compute_`/`parse_`/`is_`-named function that actually does I/O |
| `dead-island` | Warning / Advisory | Functions nothing references and that aren't declared entry points (methods → Advisory: SCIP can miss dynamic dispatch) |
| `naming-convention` | Advisory | Deviation from the codebase's dominant case style |
| `slop-name` | Advisory | Throwaway markers that outlive their intent — `_v2`, `helper_`, `temp_` |
| `over-commenting` | Advisory | Comments that merely restate the adjacent code |

Sanctioned channels come from a `slop.toml` policy. `slop init .` proposes one
from your repo's dominant patterns; without it, `infra-bypass` stays silent.
Grandfather existing findings with `slop baseline .` — this also records each
function's effect signature, so a later run can flag `effect-creep` (a function
that was pure then, doing I/O now).

## Auto-fix

`slop fix` applies the *mechanical* repairs itself — dry-run by default,
`--write` to apply:

- **over-commenting** — deletes comments that restate the adjacent line
  (behaviour-inert: comments are inert).
- **naming-convention** — renames a camelCase **free function** to snake_case,
  rewriting every SCIP-resolved reference and re-parsing each file. Aborts if
  any reference can't be located as a whole token or a file stops parsing.
  Methods, throwaway-marker names, and collisions are reported and left to a
  human — a rename there can silently break framework dispatch.

Everything else stays *guidance*: the finding carries a `fix_guidance` string
precise enough for an agent to act on.

## The agent harness

`slop` never drives edits itself — it exposes surfaces an agent host (e.g.
Claude Code) drives. Wire them into a repo with one command:

```sh
slop install /path/to/repo   # merges MCP server + hooks into the repo's config
```

- **Claude skill** — `slop install` drops a `/slop` skill so the agent knows
  when and how to check, triage, fix, and gate on its own.
- **MCP tools** — `validate_change`, `get_context_envelope`, `query_subgraph`.
- **Hooks** — read-path steering + zoned graph-distance compression (skeletonize
  code far from what you're editing; keep near context full-fidelity).
- **Gate** — `slop gate`, a CI/fix-loop entry point that exits non-zero on
  blocking findings and prints a machine-readable verdict.
- **Proxy** — `slop proxy`, an `ANTHROPIC_BASE_URL` reverse proxy for
  agent-agnostic steering + token observability.

Full details, config, and the fix-loop shape: **[docs/harness.md](docs/harness.md)**.

## Limitations

- **Python is the fully-supported language.** The graph/effect detectors
  (`infra-bypass`, `circular-import`, `dead-island`, `purity-lie`,
  `effect-layer-violation`, `effect-creep`) work over *any* SCIP-indexed
  language — `slop index` auto-selects `scip-python` or `scip-typescript`, and
  the effect seed table has a JavaScript/Node set validated against a real
  `scip-typescript` index (`axios`, `fs`, `child_process`, `process.env` all
  resolve; see the `ts_probe` fixture). The parser-based rules (duplication,
  complexity, over-commenting) work on **Python and JavaScript/TypeScript** —
  Python via ruff, JS/TS via tree-sitter (the `Language` seam in `slop-parse`).
  Resolution is only as good as the SCIP index (dynamic dispatch, `getattr`,
  duck typing can be missed).
- The read-path harness's **token wins are measured** (~−61% on dogfood repos);
  whether it preserves *output quality* is not yet rigorously proven.
- `slop fix` renames are SCIP-*verified*, not behaviour-inert — dry-run and
  review before `--write`.

## Development

```sh
cargo test           # unit + fixture-driven integration tests
cargo clippy
```

Architecture and design decisions (the `D`-numbered invariants referenced in
the code) live in `ideation/EXECUTION_PLAN.md`.
