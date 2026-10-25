# PRD P0: cut and consolidate

Roadmap: `docs/roadmap.md`, phase P0. Runs in parallel with P1, on its own
worktree and branch (`p0-cut`). Blocks P2.

## Problem

slop is six products in one binary: analyzer, write-time harness, context
engine, auto-fixer, TUI, and proxy, spread over 10 crates and about 25k lines.
The mission is now sketch, bind, lower (roadmap). Everything that does not serve
bind, reconcile or the plugin is weight that every later agent has to read
around. The repository also has no CI on push.

## Outcome

A smaller workspace that still passes every surviving test, a CLI and MCP
surface that match the mission, CI on every push, and no `ideation/` directory.
No behaviour change for the surviving commands.

## Scope

### 1. Remove (one commit per bullet)

| Remove | Where | Watch for |
| --- | --- | --- |
| Proxy | `crates/slop-proxy`, `Proxy` subcommand, `bench/proxy_ab.py` | workspace members, `Cargo.lock` |
| TUI | `crates/slop-tui`, `Dash`, hidden `Delegate` and `Open` | `Launch` stays; check it does not call the TUI |
| Fixer | `fix.rs`, `repair.rs`, `rename.rs`, `inline.rs`, `Fix` subcommand, `tests/rename_e2e.rs`, `tests/inline_e2e.rs` | `detect.rs` must keep producing `fix_guidance` text; delete only the apply path. `detect.rs:447` uses `rename::to_snake_case` for the naming rule, which is also cut |
| LLM tier3 | `crates/slop-llm`, `tier3.rs`, `--tier3` flags, the `SLOP_JUDGE` docs | `check.rs`, `detect.rs`, `tests/m1_infra_bypass.rs` reference it |
| Context envelope | `envelope.rs`, `calibrate.rs`, `relevance.rs`, `context.rs`, `compress.rs`, `Calibrate`, `Compress`, hidden `ContextBench`, MCP `get_context_envelope` and `expand_entity` | `snapshot.rs` holds `RelevanceModel` (line 16) and `envelope::EntityIndex` (line 95). If `search` or `retrieve` read `EntityIndex` or `Lexical`, move them into `search.rs` instead of deleting |
| Read compression | `handle_post_tool_use` compression and the edit-zone state in `harness.rs`; the PostToolUse entry `slop setup` writes | `slop uninstall` must still remove a PostToolUse entry written by an older setup |
| `skeleton.rs` | delete only if nothing that survives calls it | |
| Style rules | `naming-convention`, `slop-name`, `over-commenting`, `complexity-spike`, `circular-import`, `config-sprawl`, and `naming.rs`, plus their tests and fixtures | rule ids stay reserved: an old `# slop: ignore[...]` with a removed id must not error |
| `split.rs` | already unused by detection (`detect.rs:1169`) | |
| Hidden `Install` and `Claude` | compatibility commands that P3's plugin replaces | |

Keep, even though they look related: `overlay.rs` and `refresh.rs` (ADR 0006
freshness for MCP and LSP), `retrieve.rs`, `search.rs`, `prewrite.rs`,
`precheck.rs`, `health.rs`, `coverage.rs`, hidden `Equiv` (B3), `Debug`,
`Suggest`.

### 2. Collapse crates

From 10 crates to 2, after step 1 (fewer files to move):

- `slop-core`: `slop-parse`, `slop-graph`, `slop-resolve`, `slop-analyze`, as
  modules (`parse`, `graph`, `resolve`, `analyze`).
- `slop` (package `slop-cli`, binary `slop`): CLI, `mcp`, `lsp` as modules.
- Feature `egraph` moves with the code that uses it.
- `scripts/publish-crates.sh` and `.github/workflows/publish-crates.yml`
  become a single-crate publish, or are deleted if P5 defers crates.io. Delete.
- `docs/packaging.md` updated to match.

### 3. Invariants and `ideation/`

- Code cites invariants as `D<n>` in 35 files (most: D8 x15, D9 x7, D11, D12,
  D10). For each id still cited after steps 1 and 2, move its text from
  `ideation/EXECUTION_PLAN.md` into `CONTEXT.md` (a short "Invariants" section)
  or the ADR it belongs to, keeping the id.
- Then delete `ideation/`.

### 4. Benches

- Move `aider_arm.py`, `compression_bench.py`, `context_ab.py`,
  `context_bench.py`, `embedding_arm.py`, `function_ab.py`, `reuse_bench.py`,
  `split_spike.py`, `steering_ab.py`, `R8_PREREGISTRATION.md` to
  `bench/archive/`. Keep `equiv_bench.py`. Results JSONL stay where they are.
- `bench/README.md`: B3 in full; a short "Archive" section that keeps the
  headline numbers of B4, R8 runs 1 and 2 (including that R8 was negative), and
  the steering pilots, with commit ids.
- Commit the pending deletion of `bench/__pycache__/`; add `__pycache__/` and
  `.DS_Store` to `.gitignore`.

### 5. CI

`.github/workflows/ci.yml` on push and pull request: `cargo test --workspace
--locked`, `cargo clippy --workspace --all-targets --locked -- -D warnings`,
on ubuntu-24.04 and macos-14, toolchain 1.94.0 (matches `rust-version`). The
hook latency check joins in P3, when the hook set is final.

### 6. README and docs

README rules table, command list, and harness section match what survives.
`docs/harness.md` loses proxy, compression, Ollama and tier3 sections. One line
in the README points to `docs/roadmap.md`.

## Out of scope

- Any new feature, including `bind`. P2 does that.
- Renaming MCP `assess_write` or `validate_change`. P3 does that.
- Reformatting. Never run `cargo fmt` on a whole crate; format only lines you
  changed.

## Acceptance

- `cargo test --workspace --locked` and `cargo clippy --workspace --all-targets
  -- -D warnings` pass.
- `slop --help` lists exactly: `index check suggest gate setup launch uninstall
  doctor mcp lsp hook baseline init` (hidden: `equiv`, `debug`).
- MCP `tools/list` returns `find_capability`, `validate_change`,
  `assess_write`, `query_subgraph`.
- `slop check --all` on `tests/fixtures/toy_repo_slopped` gives the same
  findings as at 278c7fb, minus the removed rules. Record both outputs in the
  final commit message.
- `bench/equiv_bench.py --limit 50` reproduces B3's zero false equivalences.
- `cargo build --release` binary size and `cargo tree -e normal --prefix none |
  sort -u | wc -l` (233 at 278c7fb) reported before and after.
- `grep -rE '\bD[0-9]+\b' crates` ids all resolve to text in `CONTEXT.md` or an
  ADR.
- `ideation/`, `crates/slop-proxy`, `crates/slop-tui`, `crates/slop-llm` gone.

## Constraints

- One commit per scope bullet, each message saying what went and why. No push.
- Comments at most two lines; no emojis; no TODOs.
- Do not touch `bench/results/*.jsonl`.
- If a removal breaks a kept test in a way that suggests the kept code depended
  on removed behaviour, stop and report rather than patching around it.
