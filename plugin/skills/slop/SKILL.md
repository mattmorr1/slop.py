---
name: slop
description: Check code against the codebase it lives in with the `slop` engine: provable copies of existing functions, calls to names that do not exist, sanctioned-channel bypasses, effect-layer violations, dead code and purity lies, in Python, JS/TS or Rust. Use when the user wants to check code quality, find or clean AI-generated slop, validate a change before committing, answer a slop Stop-hook block, or explicitly says to run slop.
---

# slop: codebase-relative checks

`slop` judges code against *the repository it lives in*, an effect-typed graph of
the whole repo, not against absolute style rules. It catches the ways agent code
rots: re-adding a function that already exists, calling a helper that was never
written, bypassing the infrastructure everyone else routes through, and code
whose name lies about its effects.

## In a Claude Code session (the plugin)

- **SessionStart** records what was already dirty, so only this session's changes
  are judged, and injects the repo's channels, layers and capabilities.
- **Before a write** (warn-only, never denies) you may see `slop:` lines: a provable
  copy of an existing function, a call to a missing name with suggestions, a
  sanctioned-channel bypass, or existing code worth reading first.
- **At Stop** the turn is blocked once per provable problem in what you changed:
  an E-sound copy, a `missing` call, or an infra-bypass under policy. Each has an id.
  Fix it (call the existing function, call the suggested name, route through the
  channel), or, if it is intentional, call the `acknowledge` tool with the id and
  a one-line reason. Ranked matches listed as advisory never block.
- **Before finishing**, call the `reconcile` tool (pass the session id from session
  start) to see exactly what Stop will see, plus the full detector findings.
- `find_capability` answers "does this already exist?" before you write it;
  `query_subgraph` shows how a module routes an effect; `assess_write` checks
  proposed content without writing it.

## From the command line

1. **Index** once, and after large edits: `slop index <repo>`.
2. **Check** the working-tree diff (`slop check <repo>`) or everything (`--all`).
3. **Triage by severity**; every finding carries `fix_guidance`:
   - BLOCKING: `infra-bypass`.
   - WARNING: `duplicate-exact`, `duplicate-equivalent`, `purity-lie`,
     `effect-layer-violation`, `dead-island`.
   - ADVISORY: `duplicate-structural`, `parallel-implementation`, `untested-effect`.
4. **Act on the guidance**: route I/O through the named channel or a lower layer;
   unify duplicates behind one implementation; rename a `purity-lie` or extract
   its pure part; delete a `dead-island` or list it in `slop.toml` `entry_points`.
5. **Gate** before committing: `slop gate <repo> --fail-on warning` exits non-zero
   with a JSON verdict while slop remains.

## Notes

- With no `slop.toml`, `infra-bypass` and `effect-layer-violation` stay silent by
  design. `slop init <repo>` proposes a channel policy; `[[layer]]` rules forbid
  effects in a layer. `[agent] gate = "off" | "steer" | "reconcile"` sets how hard
  the hooks push (default `reconcile`).
- Acknowledgements live in `.slop/acks.jsonl`; commit it so reviewers see them.
