---
name: slop
description: Find and clean AI-slop in a Python, JS/TS or Rust codebase with the `slop` engine — codebase-relative duplication, infra-bypass, effect-layer violations, effect-creep, complexity spikes, dead code, purity lies, and naming drift. Use when the user wants to check code quality, find or clean AI-generated slop, validate that a change introduced no new slop before committing, or explicitly says to run slop.
---

# slop — codebase-relative AI-slop analysis

`slop` judges your code against *the codebase it lives in* — an effect-typed graph
of the whole repo — not against absolute style rules. Reach for it to catch the
ways AI-generated code rots: reimplementing code that already exists, bypassing
the infrastructure everyone else routes through, hallucinated dead code,
functions whose names lie about their effects, and tangled control flow.

## When to use
- "check this for slop", "is this AI-slop?", "clean up this code"
- before committing: confirm a change introduced no new slop
- auditing a repo's overall health

## Prerequisites
- The `slop` binary. Build it (`cargo build --release` in the slop repo →
  `target/release/slop`) or `cargo install --path crates/slop`.
- A SCIP index of the target repo: `slop index <repo>` (regenerate after
  significant edits; a check against a stale index judges old code).

## Workflow
1. **Index** (once, or after large edits): `slop index <repo>`
2. **Check** — diff by default, or the whole repo:
   - `slop check <repo>` — judges the working-tree diff vs HEAD
   - `slop check <repo> --all` — audits everything
3. **Triage by severity** (every finding carries a `fix_guidance` line):
   - **BLOCKING** — `infra-bypass`, `circular-import`, `effect-creep`. Fix these.
   - **WARNING** — `duplicate-exact`, `complexity-spike`, `purity-lie`,
     `effect-layer-violation`, `dead-island`. Should fix.
   - **ADVISORY** — `naming-convention`, `slop-name`, `over-commenting`,
     `duplicate-structural`, `semantic-redundancy`. Consider.
4. **Auto-fix the mechanical ones**: `slop fix <repo>` (dry-run) →
   `slop fix <repo> --write`. Fixes `over-commenting` (deletes comments that
   restate the code) and `naming-convention` (renames camelCase *free
   functions* to snake_case, rewriting every reference). Re-index after.
5. **Act on `fix_guidance`** for the rest, using what each rule tells you:
   - `infra-bypass` / `effect-layer-violation` → route the I/O through the named
     sanctioned channel / a lower layer, don't do it inline.
   - `duplicate-exact` / `duplicate-structural` / `semantic-redundancy` → unify
     behind one implementation and delete the copies.
   - `complexity-spike` → extract the deepest nested block it names into a helper.
   - `purity-lie` → rename to reflect the I/O, or extract the pure part.
   - `dead-island` → delete, or declare it in `slop.toml` `entry_points`.
6. **Gate before committing**: `slop gate <repo> --fail-on warning` exits
   non-zero (with a JSON verdict) if slop remains — loop until it passes.

## Notes
- No `slop.toml`? `infra-bypass` and `effect-layer-violation` stay silent by
  design. `slop init <repo>` proposes a sanctioned-channel policy; add
  `[[layer]]` rules by hand to forbid effects in a layer (e.g. no DB in a view).
- `slop baseline <repo>` grandfathers existing findings *and* records each
  function's effect signature — which is what later enables `effect-creep`
  (a function that was pure then, doing I/O now).
- Tier-3 semantic-redundancy uses an LLM judge: add `--tier3` (needs
  `ANTHROPIC_API_KEY`, or `SLOP_JUDGE=ollama` for a local model).
- If slop's MCP server is registered (via `slop install`), the
  `validate_change`, `query_subgraph`, and `get_context_envelope` tools are
  available directly — prefer `validate_change` over shelling out mid-session.
