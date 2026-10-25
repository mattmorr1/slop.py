# The slop harness (v2)

`slop` never edits code. Changes are driven by the configured agent host,
which slop feeds through three surfaces:

1. **MCP tools** — `find_capability`, `assess_write`, `validate_change`, `query_subgraph`.
2. **Hooks** — write-time duplicate checks, sanctioned-channel steering, and the session world model.
3. **Gate** — `slop gate`, the CI/loop entry point (see below).

All of it reuses the same analysis engine `slop check` runs — no reimplementation.

Build the binary first: `cargo build --release` → `target/release/slop`.

### One-command setup

```
slop setup <repo-root> --ai claude --editor cursor
# or
slop setup <repo-root> --ai codex --editor zed
```

wires slop into the selected agent host and saves the AI/editor preference in
the user's slop config. Claude setup merges the MCP server into `.mcp.json`,
the write/prompt/session hooks into `.claude/settings.json`, and drops the **slop skill**
at `.claude/skills/slop/SKILL.md` (an agent
playbook for `/slop` — check, triage, fix, gate), pointing every entry at the
binary you invoked. Codex setup adds a delimited project-scoped MCP block to
`.codex/config.toml`; Codex CLI, the IDE extension, and the desktop app share
that trusted-project configuration. Setup is transactional and **idempotent**:
re-running updates slop's entries and preserves every other MCP server, hook,
and Codex TOML byte-for-byte. It
refuses to touch a config file that isn't valid JSON unless you pass `--force`.
`slop uninstall` structurally removes only owned entries; `slop doctor` checks
the host, editor, receipt, project markers, and index. Restart the agent host
after setup.

---

## 1. MCP tools

Newline-delimited JSON-RPC 2.0 over stdio. Launch per-project:

```
slop mcp <repo-root> [--index path/to/index.scip]
```

Register in `.mcp.json` (project scope) or `~/.claude.json`:

```json
{
  "mcpServers": {
    "slop": {
      "type": "stdio",
      "command": "/abs/path/to/target/release/slop",
      "args": ["mcp", "/abs/path/to/your/repo"]
    }
  }
}
```

Tools default the `repo` argument to the launch repo; each accepts an explicit
`repo` override. `validate_change` needs SCIP coverage; generate every detected
language artifact with:

```
slop index <repo>
```

| Tool | Purpose | Key args |
| --- | --- | --- |
| `find_capability` | What the codebase already provides for an intent | `intent`, `effect`, `limit` |
| `assess_write` | Proposed-source policy findings and existing homes | `file`, `content`, `limit` |
| `validate_change` | Full detector suite → findings + fix guidance | `all`, `base` |
| `query_subgraph` | Callers/callees/imports + effect signature | `entity`, `depth`, `edge_kinds` |

`find_capability` is the intent-first entry point: the graph tools take an entity
id, and until now nothing produced one — the graph's only lookup was an exact
match on an id the agent had no way to guess. Ask it what exists before writing
a new implementation, then feed any id it returns to `query_subgraph`. Ranking is deterministic (name / module / docstring
token overlap, ties broken by how many places call it): no embeddings, no LLM.

---

## 2. Hooks

Per-write hooks never build the graph. Session start emits a compact,
schema-versioned callee-neighborhood sidecar into the system temp directory;
later writes combine it with the current `slop.toml` policy.

- `slop hook pre-tool-use` — **write-time steering**: judges the content a
  `Write`/`Edit`/`MultiEdit` is *about to* produce, before it lands, and attaches
  `additionalContext` naming the sanctioned channel it bypasses and any existing
  function whose distinctive callees make it a plausible home for the code.
  - It parses the proposed text once. Direct effect acquisition is checked
    against policy; reuse retrieval reads the compact sidecar rather than SCIP
    or the full graph. Missing sidecar degrades to policy-only steering.
  - An `Edit`'s `new_string` is a dedented fragment that wouldn't parse alone, so
    the replacement is applied to the on-disk file first and the *result* checked.
  - **Warn-only.** It never sets `permissionDecision`, so it cannot block a write.
    Denying is gated on measured per-rule precision — a false deny costs more than
    a missed finding. Promote a rule only once its precision earns it.
  - Policy findings stay silent when the repo has no confirmed channels (D8),
    while reuse evidence can still surface. Unsupported languages stay silent.
  - Sees *direct* acquisition only (no graph ⇒ no transitive propagation). The
    deeper checks stay in `validate_change` / `slop gate`.
  - **Provable duplicates.** A new or changed function (keyed by name and
    E-sound hash, so untouched code never matches itself) that is provably
    equivalent to an existing one is named first, with its location. A pure
    rename is not reported. Python only; warn-only like everything else here.
  - **Reuse with usage.** The top two reuse suggestions carry the existing
    function's body (at most 30 lines, read from disk and checked to still be
    that function) and one real call site from a caller. R8 found models reuse
    what they see *used*, not what they see *named*.

Every hook call appends one line to `<repo>/.slop/hooks.jsonl` (event, file,
latency, injected tokens, duplicates and reuse suggestions fired) for measuring
the harness in daily use. `SLOP_HOOK_LOG=0` turns it off.

- `slop hook post-tool-use` — attaches sanctioned-channel steering to a `Read`
  of a supported source file. Setup no longer registers it (and removes an old
  registration); it is kept so installs from before read compression was removed
  keep working.
- `slop hook user-prompt-submit` — injects the repo's sanctioned-channel
  policy plus a pointer to `validate_change` as pre-hoc steering.
- `slop hook session-start` / `slop hook subagent-start` — injects the
  **world model**: the codebase facts an agent needs *before* it designs
  anything, rather than as a correction afterwards.
  - Sanctioned channels, layer rules, and a **capability index** — the
    codebase's most-referenced functions grouped by effect, so the agent reuses
    what exists instead of writing a parallel version. Redundancy is the most
    common slop class and this is the cheapest prevention for it.
  - This is the one hook that can afford a graph build, because it fires once
    per session rather than per tool call (~190ms on this repo). Without an
    index it degrades to policy-only facts instead of failing.
  - Registered for subagents too: they otherwise start with no codebase
    context whatsoever.
  - Budgeted to 7000 chars against the host's 10k `additionalContext` cap;
    capabilities are trimmed first, so the policy facts always survive.
  - Silent when there is nothing to say (no policy and no graph).

Register in `.claude/settings.json` (or just run `slop setup --ai claude`, which
writes these plus the `SessionStart` and `SubagentStart` entries):

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Write|Edit|MultiEdit",
        "hooks": [
          { "type": "command", "command": "/abs/path/to/target/release/slop hook pre-tool-use" }
        ]
      }
    ],
    "UserPromptSubmit": [
      {
        "hooks": [
          { "type": "command", "command": "/abs/path/to/target/release/slop hook user-prompt-submit" }
        ]
      }
    ]
  }
}
```

---

## 3. Gate

`slop gate <repo>` is the validation gate for a CI step or an agent fix-loop.
It runs the full check and **exits non-zero when any finding is at or above the
`--fail-on` threshold** (default: Blocking), printing machine-readable JSON
(findings + `fix_guidance`) the driving agent loops on. slop reports; the agent
edits.

```
slop gate <repo> [--base HEAD] [--all] [--fail-on blocking|warning|advisory] [--reindex] [--worktree]
```

- `--fail-on warning` makes the gate fail on Warnings too — this is what lets
  the loop act on `duplicate-exact`, `duplicate-equivalent` and `purity-lie`, not
  just the deterministic blockers (`infra-bypass`, `effect-creep`). The JSON
  reports `fail_on`, `failing` (count at/above threshold), and `blocking`.
- `--reindex` regenerates every detected language's SCIP artifact before checking,
  so a re-run after edits sees the new graph.
- `--worktree` runs inside an isolated `git worktree` of the repo.

Agent fix-loop shape (driven by the host, not slop):

```
loop:
  slop gate <repo> --reindex   # exits 0 → done
  → feed the JSON findings back to the agent
  → agent edits
```

