# slop

A **codebase-relative AI-slop analyzer** for Python, JavaScript/TypeScript and
Rust, plus an agent harness that steers a coding agent away from slop as it
writes.

Most linters judge a file against absolute rules. `slop` judges code against
*the codebase it lives in*: it builds an effect-typed graph of the whole repo
and flags the things that make AI-generated code rot — reimplementing something
that already exists, bypassing the infrastructure everyone else routes through,
hallucinated dead code, functions whose names lie about what they do, and
tangled control flow — then either fixes them mechanically or hands an agent
precise, located guidance.

> Status: **v0**, built on a SCIP index. The
> detectors and harness are dogfooded on real repos (see below); the
> token/quality *value* of the read-path harness is measured but not yet
> rigorously proven. See [Limitations](#limitations).

## Install

Release archives contain one binary for Linux/macOS on x86-64/ARM64, plus a
checksum, README, and license. Building from source requires a recent Rust
toolchain. Python/TypeScript indexing also requires `npx`; Rust uses
`rust-analyzer`.

```sh
cargo install --path crates/slop
# or, without installing:
cargo build --release                # -> target/release/slop
```

The crates.io package is `slop-cli` (the binary remains `slop`); once the
0.1.0 publish workflow completes, install it with `cargo install slop-cli`.

## Quickstart

`slop` reads a [SCIP](https://github.com/sourcegraph/scip) index of your repo.
Generate one, then check:

```sh
slop setup              # choose Claude/Codex and an editor; install the harness
slop doctor             # verify host, editor, harness, project, and index state
slop index              # indexes every detected language family
slop check              # judges the working-tree diff vs HEAD (cwd)
slop analyze --all      # visible alias for whole-repository analysis
slop check --all        # audits the whole repo
slop check --reindex    # regenerate the index first (else a stale one warns)
slop check --json       # machine-readable output for editors / CI
```

Every command defaults its repo argument to the current directory. (`slop index`
wraps pinned `scip-python` 0.6.6, pinned `scip-typescript` 0.4.0, or
`rust-analyzer scip`, detected from the source tree; polyglot repositories get
one artifact per language and one combined graph. `--indexer` or `--index`
retains the single-artifact seam. A broken/empty index fails loudly
rather than silently passing.)

Every machine-readable judgment names the immutable repository snapshot and its
freshness. CLI, gate, MCP, LSP, TUI, baseline, and fix all project from that
same evaluated finding population; refreshing the dashboard swaps its graph and
findings together.

```
WARNING (1)
  [complexity-spike] api.claude::chat  backend/api/claude.py:318
    `chat` is tangled: 31 control-flow branches nested 4 deep (cyclomatic 55)
    fix: Extract the deepest block (around line 428, nested 4 deep) into a
         named helper, or flatten it with early-return guard clauses

1 finding(s) — 0 blocking, 1 warning, 0 advisory
health: 84/100
```

Findings are grouped by severity and colorized on a terminal (piping stays
plain; `NO_COLOR` is honored). Every run ends with a **health score** (0–100),
and `slop check` exits non-zero when anything is *Blocking*.

### Interactive dashboard

Run `slop dash [repo]` to open the optional full-screen TUI. Bare `slop` prints
the command surface, so setup and scanning remain discoverable without learning
dashboard keybindings first.

- browse the whole-repo audit with the arrow keys; each finding's fix guidance
  shows in a detail pane
- **`Enter`** opens the selected finding in your editor at its line
  (`$SLOP_EDITOR`, else `cursor`/`code`)
- **`x`** applies the mechanical repair when the finding is auto-fixable
  (over-commenting / naming) — deterministic, no LLM
- **`a`** dispatches the finding to **Claude Code** (headless, `acceptEdits`)
  seeded with its fix guidance — for the semantic findings the mechanical fixer
  can't touch. Both `x` and `a` then reindex + gate to verify the finding
  actually cleared, so *every* finding has a fix path from the dashboard
- **`e`** opens the **graph explorer** on the selected finding: its effect
  signature and 1-hop edges (what it calls / imports, and who calls it) from the
  SCIP graph. `Enter` jumps focus to a neighbor, `Backspace` goes back — walk the
  call graph to understand code, not just lint it
- **`y`** copies the selected finding's full detail (rule, entity, `file:line`,
  message, fix) to the clipboard — no fighting the terminal's row-wise mouse
  selection to grab multiline text out of the detail box
- **`v`** verifies: runs `slop gate` and shows a PASS/FAIL verdict panel with
  the health + failing/blocking counts — the CI check, in place
- **`c`** opens a command palette to run any slop command — re-index, check,
  fix, baseline, init policy, install harness, gate, run the project's tests —
  after which the dashboard reloads and the status shows the `blocking N→M ·
  health A→B` delta, so you watch the finding actually clear (the verification
  loop)
- **`f`** filters by severity, **`g`** toggles grandfathered findings, **`r`**
  reloads

Health is scored two ways — everything vs. only un-grandfathered — so a
baselined repo doesn't look falsely pristine.

## What it flags

| Rule | Severity | What it catches |
| --- | --- | --- |
| `infra-bypass` | Blocking | Acquiring a raw effect (net/fs/db/env) directly when the codebase routes that effect through a sanctioned channel |
| `circular-import` | Blocking | Import cycles (Tarjan SCC over the import graph) |
| `effect-layer-violation` | Warning | An entity in a declared layer directly does an effect that layer forbids (e.g. DB/net in a `pure-utils` or presentation layer) |
| `effect-creep` | Blocking | A function that was **pure** at `slop baseline` time now performs I/O — a purity regression (delta vs baseline) |
| `duplicate-exact` | Warning | Functions with identical bodies (modulo comments/whitespace) |
| `duplicate-structural` | Advisory → Warning | Same control-flow shape, renamed vars/literals — a *candidate*; `--tier3` promotes ones an LLM judge confirms |
| `parallel-implementation` | Advisory | Functions calling the same *distinctive* set of things while sharing no code — one job implemented twice, in different words. Duplication found on graph shape, where the `duplicate-*` rules find it on token shape |
| `complexity-spike` | Warning | Genuinely tangled functions — deep nesting or many independent branches, not just a fat boolean guard |
| `purity-lie` | Warning | A `compute_`/`parse_`/`is_`-named function that actually does I/O |
| `untested-effect` | Advisory | A branching function that performs I/O and no test reaches, in a codebase that tests most of its effectful functions |
| `dead-island` | Warning / Advisory | Functions nothing references and that aren't declared entry points (methods → Advisory: SCIP can miss dynamic dispatch) |
| `naming-convention` | Advisory | Deviation from the codebase's dominant case style |
| `slop-name` | Advisory | Throwaway markers that outlive their intent — `_v2`, `helper_`, `temp_` |
| `config-sprawl` | Advisory | One environment variable read straight from the environment across four or more modules — a config surface with no single definition |
| `over-commenting` | Advisory | Comments that merely restate the adjacent code |

Sanctioned channels come from a `slop.toml` policy. `slop init .` proposes one
from your repo's dominant patterns; without it, `infra-bypass` stays silent.
Grandfather existing findings with `slop baseline .` — this also records each
function's effect signature, so a later run can flag `effect-creep` (a function
that was pure then, doing I/O now).

## Auto-fix

`slop fix` applies the *mechanical* repairs itself — dry-run by default,
`--write` to apply:

- **naming-convention** — renames a camelCase **free function** to snake_case,
  rewriting every SCIP-resolved reference and re-parsing each file. Aborts if
  any reference can't be located as a whole token or a file stops parsing.
  Methods, throwaway-marker names, and collisions are reported and left to a
  human — a rename there can silently break framework dispatch.
- **over-commenting** — available only with `--allow-advisory`; directives and
  tooling comments are protected, but natural-language classification is still
  graded and must be reviewed.
- **dead-island** and judge-confirmed wrappers — destructive repairs require
  their explicit opt-in flags.

Every repair plan is bound to a repository snapshot, checks source preconditions,
and re-parses staged results. Original files remain in a rollback journal while
slop reindexes and verifies the selected finding ID disappeared; verification
failure restores both source and index before returning an error. Use
`--finding <id>` to repair one exact finding from `slop check --json`.

Everything else stays *guidance*: the finding carries a `fix_guidance` string
precise enough for an agent to act on.

## The agent harness

Configure Claude or Codex plus the editor `slop launch` should open:

```sh
slop setup /path/to/repo --ai codex --editor cursor
slop launch /path/to/repo
slop uninstall /path/to/repo # structurally removes only slop-owned entries
```

Claude gets MCP, hooks, and the embedded skill. Codex gets a project-scoped,
delimited MCP block in `.codex/config.toml`; the rest of that TOML is preserved
byte-for-byte. AI/editor preferences remain user-local. `slop install` and
`slop claude` remain compatibility commands:

```sh
slop claude                  # install harness + launch claude in the cwd
slop claude --proxy          # also route the session through slop's steering proxy
slop claude -- --resume      # args after `--` pass through to claude
```

- **Claude skill** — `slop install` drops a `/slop` skill so the agent knows
  when and how to check, triage, fix, and gate on its own.
- **MCP tools** — `find_capability` (what does this repo already have?),
  `assess_write` (where proposed code belongs), `validate_change`,
  `get_context_envelope`, and `query_subgraph`. Context envelopes
  carry snapshot provenance, deterministic integer score reasons, and explicit
  verbatim fallback when index coverage is stale.
- **Hooks** — read-path steering + zoned graph-distance compression (skeletonize
  code far from what you're editing; keep near context full-fidelity).
- **Gate** — `slop gate`, a CI/fix-loop entry point that exits non-zero on
  blocking findings and prints a machine-readable verdict.
- **Proxy** — `slop proxy`, a bounded `ANTHROPIC_BASE_URL` reverse proxy with
  explicit concurrency, request-size, capture-size, and timeout ceilings.

Full details, config, and the fix-loop shape: **[docs/harness.md](docs/harness.md)**.

## Editor integration

`slop lsp` is a language server that publishes findings as inline diagnostics on
open/save — Blocking → Error, Warning → Warning, Advisory → Information, each
with its fix guidance. It's editor-agnostic (VS Code, Cursor, Neovim, Zed,
JetBrains all speak LSP); point your editor's LSP client at `slop lsp`. A ready
VS Code / Cursor shim lives in **[editors/vscode](editors/vscode)**.

```sh
slop lsp                # serve over stdio for the cwd (what an editor launches)
```

## Limitations

- **Python, JavaScript/TypeScript and Rust are supported**, through one seam
  (`Language` in `slop-parse`): Python via ruff, the rest via tree-sitter.
  `slop index` runs `scip-python`, `scip-typescript`, and/or
  `rust-analyzer scip` for every detected language, and the effect seed table
  carries a validated set for each. Every rule runs on all three. Resolution is only as good as the SCIP
  index — dynamic dispatch, `getattr` and duck typing can be missed, and a
  *stale* index is worse than a missing one, so `check`/`gate` regenerate it
  rather than judging a diff against yesterday's graph.
- The read-path harness's **token wins are measured** (~−61% on dogfood repos);
  whether it preserves *output quality* is not yet rigorously proven.
- `slop fix` renames are SCIP-*verified*, not behaviour-inert — dry-run and
  review before `--write`.
- The **LSP server** (`slop lsp`) is covered by headless tests; the **VS Code
  shim** in `editors/vscode` is not — exercising it needs a running extension
  host. It's kept to a thin launcher so the untested surface stays minimal.

## Development

```sh
cargo test           # unit + fixture-driven integration tests
cargo clippy
```

Architecture and design decisions (the `D`-numbered invariants referenced in
the code) live in `ideation/EXECUTION_PLAN.md`.
