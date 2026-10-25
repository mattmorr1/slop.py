# slop

A static analyzer for Python, TypeScript/JavaScript and Rust that judges code
against the repository it lives in, plus a harness that feeds those judgments to
a coding agent while it writes.

It builds a graph of the repo from a [SCIP](https://github.com/sourcegraph/scip)
index, tags each function with the effects it performs (net, fs, db, env), and
flags reimplementations, bypassed infrastructure, dead code and misleading
names.

Status: v0. The detectors are dogfooded on real repositories. Where it is going
(sketch, bind, lower) is in [docs/roadmap.md](docs/roadmap.md).

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
slop suggest            # where proposed code (on stdin) belongs
slop baseline           # grandfather current findings
slop init               # propose a slop.toml from dominant patterns
```

```
BLOCKING (1)
  [infra-bypass] services.alerts::send_alert  services/alerts.py:11
    `services.alerts::send_alert` acquires a raw Net effect directly via `urllib.request`;
    this codebase routes Net I/O through core.http_client.HttpClient
    fix: Delegate the Net operation to the sanctioned channel `core.http_client.HttpClient`
         instead of using `urllib.request` directly

1 finding(s) — 1 blocking, 0 warning, 0 advisory
```

A stale index is regenerated rather than trusted (`--reindex` forces it). Every
JSON result names the repository snapshot it was computed from.

## Rules

| Rule | Severity | Flags |
| --- | --- | --- |
| `infra-bypass` | Blocking | Raw net/fs/db/env access where the repo has a sanctioned channel for it |
| `effect-creep` | Blocking | A function that was pure at baseline now does I/O |
| `effect-layer-violation` | Warning | An effect a declared layer forbids |
| `duplicate-exact` | Warning | Identical bodies, ignoring comments and whitespace |
| `duplicate-equivalent` | Warning | Python functions provably equal up to renaming and sound rewrites ([ADR 0004](docs/adr/0004-e-equivalence-normalizer-with-egglog-as-oracle.md)) |
| `duplicate-structural` | Advisory | Same shape, different names |
| `parallel-implementation` | Advisory | Same distinctive callees, no shared code |
| `purity-lie` | Warning | `compute_`/`parse_`/`is_` functions that do I/O |
| `dead-island` | Warning | Unreferenced, non-entry-point code (Advisory for methods) |
| `untested-effect` | Advisory | Branching I/O no test reaches, in a repo that usually tests it |

`infra-bypass` needs a `slop.toml` policy; `slop init` proposes one from the
repo's dominant patterns. `slop baseline` grandfathers current findings and
records effect signatures for `effect-creep`. Rule ids removed in P0
(`circular-import`, `complexity-spike`, `config-sprawl`, `naming-convention`,
`slop-name`, `over-commenting`) stay reserved: old suppressions naming them are
accepted and do nothing.

## Claude Code

Install the plugin from inside Claude Code:

```
/plugin marketplace add mattmorr1/slop.py
/plugin install slop@slop
```

The plugin runs the `slop` binary it finds: `$SLOP_BIN`, then a `slop` of the
same version on `PATH`, then `uvx --from slop-cli`. Index the repo once with
`slop index` so the hooks have something to compare against.

**What blocks.** At the end of a turn, the Stop hook checks only what this
session changed and blocks once per *provable* problem:

- a new function that is provably equivalent to one the repo already has;
- a call to a name that provably does not exist (with "did you mean"
  suggestions);
- new raw network, file, database or env access that bypasses the channel
  `slop.toml` sanctions.

**What never blocks.** Writes: PreToolUse only adds context before a write lands
(the same three checks plus existing code worth reusing). Ranked matches such as
a similar shape or overlapping calls, files that were already dirty when the
session started, and any internal slop error. Each block names its finding once,
and a second Stop in the same turn is always allowed.

**Acknowledging.** Each finding has an id. If it is intentional, the agent calls
the `acknowledge` tool with the id and a reason. That appends to
`.slop/acks.jsonl`, and the id stops blocking; commit the file so reviewers see
the reasons. The `reconcile` tool shows what Stop will say before it says it.

`[agent] gate = "off" | "steer" | "reconcile"` in `slop.toml` turns the hooks
off, keeps steering without the Stop gate, or keeps both (the default).
`slop setup --ai claude` removes what older versions wrote into the repo and
prints the two commands above.

## Other agent hosts

```sh
slop setup --ai codex --editor cursor   # MCP entry in .codex/config.toml
slop launch
slop doctor                             # check the saved host, receipt and index
slop uninstall                          # removes only what setup wrote
```

- **MCP** (`slop mcp`): `find_capability`, `assess_write`, `query_subgraph`,
  `reconcile`, `acknowledge`.
- **Gate** (`slop gate`): the CI and agent fix-loop entry point.

slop never edits code itself; findings carry `fix_guidance` for the agent.

Details: [docs/harness.md](docs/harness.md).

## Editors

`slop lsp` publishes findings as diagnostics on open and save (Blocking = Error,
Warning = Warning, Advisory = Information). Any LSP client works;
[editors/vscode](editors/vscode) has a VS Code/Cursor launcher.

## Limitations

- Resolution is as good as the SCIP index: dynamic dispatch, `getattr` and duck
  typing can be missed.
- `duplicate-equivalent` is Python-only; the other parser-backed rules cover
  Python, JS/TS and Rust.
- The VS Code launcher has no tests; the LSP server does.

## Development

```sh
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Decisions are in [docs/adr](docs/adr); the invariants the code cites as `D<n>`
are in the Invariants section of [CONTEXT.md](CONTEXT.md).
