# PRD P3: Claude Code plugin and reconcile gate

Roadmap: `docs/roadmap.md`, section "Re-plan after G1". Runs in parallel with
P5 on its own worktree (`p3-plugin`).

## Problem

slop's checks only change what Claude ships if Claude cannot skip them. MCP
tools are optional and get skipped under task pressure; today's hooks only
steer. And the Claude wiring is a settings merge done by `slop setup`, which is
a worse install path than a plugin.

## Outcome

One install, `/plugin marketplace add mattmorr1/slop` then `/plugin install
slop@slop`, after which Claude cannot end a turn while the code it changed
this session contains an unacknowledged **provable** fact:

1. a function E-sound-equal (or exactly equal) to an existing one outside the
   changed set;
2. a call to a name that does not exist (Python, v1);
3. an `infra-bypass` under a confirmed `slop.toml` channel.

Ranked matches (structural duplicates, parallel implementations, "did you mean"
suggestions for unresolved names) are attached to the same message as advice
and never cause a block on their own.

## Scope

### 1. Plugin layout (in this repository)

```
.claude-plugin/marketplace.json        name "slop", one plugin, source ./plugin
plugin/.claude-plugin/plugin.json      name, version, mcpServers.slop
plugin/hooks/hooks.json                SessionStart, PreToolUse(Write|Edit|MultiEdit), Stop
plugin/bin/slop-run                    POSIX sh launcher (below)
plugin/skills/slop/SKILL.md            the playbook
```

Model it on the airlock plugin (`~/.claude/plugins/cache/mongohack/airlock/0.2.0`):
`mcpServers` inline in `plugin.json`, `${CLAUDE_PLUGIN_ROOT}` in commands,
explicit hook `timeout`s.

- **Skill:** `plugin/skills/slop/SKILL.md` becomes the single source.
  `crates/slop/assets/slop-skill.md` is deleted, and the binary embeds the
  plugin file with `include_str!`, so the two can never drift.
- **Launcher `slop-run`:** resolves the binary once and caches the absolute
  path in `${TMPDIR:-/tmp}/slop-plugin-<version>.path`. Resolution order:
  1. `$SLOP_BIN`;
  2. `slop` on PATH when `slop --version` equals the plugin version;
  3. `uvx --from slop-cli==<version> slop`, resolved to a real path once.

  Every later call execs the cached path. Option 3 depends on P5; until P5
  publishes, 1 and 2 must work, and 3 fails with a clear message naming
  `SLOP_BIN`.

### 2. Session baseline

The Stop gate judges what changed **this session**, not the user's
pre-existing dirty tree.

- SessionStart records HEAD plus a content hash (blake3) of every dirty or
  untracked file that slop indexes, under `.slop/session/<session_id>.json`.
- At Stop, the changed set is every file that differs from its baseline: from
  HEAD for files clean at start, from the recorded hash for files dirty at
  start.
- SessionStart also writes the prewrite sidecar (today's behaviour), extended
  with what Stop needs: the E-sound index (`prewrite::equivalence_index`), the
  repository's defined-name table (module to top-level names, class to
  methods), and the policy. Stop must never build the graph or read SCIP.

### 3. Unresolved-name check (`missing`), Python only

A call `f(...)`, `mod.f(...)` or `Class.f(...)` in changed code is `missing`
when `f` is not:

- a builtin,
- defined or assigned in an enclosing scope (including parameters,
  comprehension targets, `global` and `nonlocal`, and class bodies),
- imported, or
- for `mod.f` where `mod` is an imported repository module, defined in that
  module per the sidecar.

To stay provable, skip the whole file when it has a star import, a
module-level `__getattr__`, or `exec` or `eval`. Skip any attribute call on a
value that is not a module (`obj.method()`), since types are unknown.

Each `missing` finding carries up to three advisory suggestions: BM25 over the
sidecar's defined names, plus the module that defines each. This is where B5's
cross-file signal lives.

### 4. Stop hook (L2 reconcile gate)

- Input: the Claude Code Stop payload (`session_id`, `cwd`,
  `stop_hook_active`).
- If `stop_hook_active` is true, allow. Otherwise compute the changed set and
  the three provable checks on changed functions and changed lines only. Drop
  acknowledged ids and ids already blocked once in this session (record them
  in the session file).
- If any remain, return `{"decision": "block", "reason": ...}`. The reason
  lists at most 8 findings, each with:
  - a stable id (rule plus E-sound hash or name plus file),
  - `file:line`,
  - the fact in one line,
  - the action: call `X` at `path:line` instead; define or import `Y`; route
    through `Z`.

  It then lists the advisory items, and ends with: "If this is intentional,
  call the slop `acknowledge` tool with the id and a reason."
- **Fail open.** Any internal error allows the stop, writes the error to
  stderr and to `.slop/hooks.jsonl`, and never blocks Claude. A slop bug must
  never trap a session.
- Config in `slop.toml`:

  ```toml
  [agent]
  gate = "off" | "steer" | "reconcile"   # default "reconcile"
  ```

  `steer` keeps PreToolUse context only.

### 5. PreToolUse (L1)

Keep today's write-time steering, and add `missing` findings for the proposed
content, with suggestions. It stays warn-only and never sets
`permissionDecision`. Fold UserPromptSubmit's channel steering into
SessionStart and drop the UserPromptSubmit hook. Drop the PostToolUse handler
if nothing the plugin uses still depends on it.

### 6. MCP

- Tools: `find_capability`, `assess_write`, `query_subgraph`, `reconcile`
  (renamed from `validate_change`; scoped to the session's changed set when
  called with a `session_id`, else the working-tree diff), and
  `acknowledge(id, reason)`.
- `acknowledge` appends `{ts, session_id, id, reason}` to
  `.slop/acks.jsonl`. Acks are repository-scoped and persist across sessions.

### 7. `slop setup`

`--ai claude` stops writing hooks and MCP entries, and prints the two plugin
commands instead. `slop uninstall` still removes entries written by an older
setup. Codex wiring is unchanged.

### 8. Docs

- README: a "Claude Code" section with the install commands, what the gate
  blocks, what it never blocks, and how `acknowledge` works.
- `docs/harness.md`: updated to match.

## Out of scope

- PreToolUse `deny` (L4) and sketch-strict (L3). Deferred; the user wants
  intrusive later.
- `missing` for TypeScript and Rust.
- Publishing anything (marketplace push, PyPI). Local only.

## Acceptance

- `cargo test --workspace --locked` and `cargo clippy --workspace
  --all-targets --locked -- -D warnings` pass.
- **Zero false `missing` by construction.** Running the `missing` check over
  every non-test Python file at HEAD of flask, httpx, requests, click and vigil
  (github.com/Vigil-SOC/vigil) reports 0 findings. Code at HEAD runs, so every
  finding there is a false positive. Report the per-repo file counts and
  skipped-file counts, and fix the check (not the corpus) until it is 0.
- **End-to-end hook test** with recorded payloads on
  `tests/fixtures/toy_repo_slopped` (SessionStart, PreToolUse, Stop), all of
  which must hold:
  - a session that adds an E-sound copy of an existing function is blocked
    once, and allowed on the second Stop;
  - a session that adds a call to a nonexistent helper is blocked, and the
    reason names a suggestion;
  - an acknowledged id does not block;
  - a pre-existing dirty file outside the session is ignored;
  - a hook given a corrupt sidecar fails open.
- **Latency:** Stop p95 under 500 ms and PreToolUse p95 under 150 ms, warm, on
  vigil with a 3-file session. Report p50 and p95; add the fixture-sized check
  to CI.
- **Plugin smoke:** with `SLOP_BIN` pointing at a release build, the plugin
  loads in Claude Code from a local marketplace path, the MCP server lists its
  five tools, and a scripted session hits the Stop block. If `claude plugin
  validate` exists, run it. Report what was verified by hand and what was not.
- **Honest record:** on vigil, re-add one of the known `slugify` copies in a
  new file during a session, and report exactly what Stop says. Not a gate.
  Expectation: only advisory structural matches, since the copies are not
  E-sound-equal.

## Constraints

- Step commits with rationale, no push. Never `cargo fmt` a whole crate.
  Comments at most 2 lines, no emojis, no placeholders. Errors are logged,
  never swallowed; failing open is a logged decision, not a silent one.
- Stop and PreToolUse never build the graph or read SCIP.
- If the `missing` check cannot reach zero false positives on the corpus
  without skipping most files, stop and report the skip rate rather than
  shipping it as a blocking check.
