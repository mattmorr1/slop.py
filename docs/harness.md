# The slop harness (v2)

`slop` never drives edits itself (D9). It exposes three surfaces an existing
agent host (e.g. Claude Code) drives:

1. **MCP tools** — `validate_change`, `get_context_envelope`, `query_subgraph`.
2. **Hooks** — read-path compression + sanctioned-channel steering.
3. **Gate** — `slop gate`, the CI/loop entry point (see below).
4. **Proxy** — `slop proxy`, agent-agnostic steering/observability (see below).

All of it reuses the same analysis engine `slop check` runs — no reimplementation.

Build the binary first: `cargo build --release` → `target/release/slop`.

### One-command setup

```
slop install <repo-root>
```

wires slop into a repo's agent host for you: it merges the MCP server into
`<repo>/.mcp.json`, the read/prompt hooks into `<repo>/.claude/settings.json`,
and drops the **slop skill** at `<repo>/.claude/skills/slop/SKILL.md` (an agent
playbook for `/slop` — check, triage, fix, gate), pointing every entry at the
binary you invoked (absolute path). It is
**idempotent** — re-running updates slop's own entries (e.g. after a rebuild
moves the binary) and preserves every other MCP server and hook you have. It
refuses to touch a config file that isn't valid JSON unless you pass `--force`.
Sections 1–2 below document the config it writes, if you'd rather do it by hand.
Restart the agent host afterwards to load the server and hooks.

### Local LLM (Ollama)

Anywhere slop needs an LLM it can use a local Ollama model instead of the
Claude API — free, offline, no key:

- **Tier-3 judging**: `SLOP_JUDGE=ollama slop check <repo> --tier3` (or via
  `validate_change`). Selects the local backend; otherwise Claude is used.
- **Skeleton densification**: `slop compress … --densify` adds a one-line
  semantic summary above each skeleton (pure gain on undocumented functions).
- Config: `OLLAMA_HOST` (default `http://localhost:11434`), `OLLAMA_MODEL`
  (default `qwen2.5:1.5b`). Requires `ollama serve` running.

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
`repo` override. `validate_change` needs a SCIP index (`<repo>/index.scip` by
default); generate one with:

```
npx --yes @sourcegraph/scip-python index <repo> --project-name <name> --output <repo>/index.scip
```

| Tool | Purpose | Key args |
| --- | --- | --- |
| `find_capability` | What the codebase already provides for an intent | `intent`, `effect`, `limit` |
| `validate_change` | Full detector suite → findings + fix guidance | `all`, `base`, `tier3` |
| `get_context_envelope` | Effect-typed, budget-packed context around an entity | `target_entity`, `token_budget`, `edit_zone_hops` |
| `query_subgraph` | Callers/callees/imports + effect signature | `entity`, `depth`, `edge_kinds` |

`find_capability` is the entry point: the other two graph tools take an entity
id, and until now nothing produced one — the graph's only lookup was an exact
match on an id the agent had no way to guess. Ask it what exists before writing
a new implementation, then feed any id it returns to `query_subgraph` or
`get_context_envelope`. Ranking is deterministic (name / module / docstring
token overlap, ties broken by how many places call it): no embeddings, no LLM.

---

## 2. Hooks

All three hooks derive steering from `slop.toml` alone (no SCIP index, no graph
build) so they are cheap enough to run on every read/write/prompt.

- `slop hook pre-tool-use` — **write-time steering**: judges the content a
  `Write`/`Edit`/`MultiEdit` is *about to* produce, before it lands, and attaches
  `additionalContext` naming the sanctioned channel it bypasses.
  - Content-local by design: it parses the proposed text, takes the qualified
    names it references, and asks the policy whether an effect it acquires
    already has a channel. No index, no graph — ~7ms including process spawn.
  - An `Edit`'s `new_string` is a dedented fragment that wouldn't parse alone, so
    the replacement is applied to the on-disk file first and the *result* checked.
  - **Warn-only.** It never sets `permissionDecision`, so it cannot block a write.
    Denying is gated on measured per-rule precision — a false deny costs more than
    a missed finding. Promote a rule only once its precision earns it.
  - Silent when the repo has no `slop.toml` channels (D8), and for files in no
    supported language.
  - Sees *direct* acquisition only (no graph ⇒ no transitive propagation). The
    deeper checks stay in `validate_change` / `slop gate`.

- `slop hook post-tool-use` — maintains a **session edit zone** and does
  **zoned graph-distance compression** on reads:
  - On a `Write`/`Edit`/`MultiEdit` of a file in any supported language
    (Python, JS/TS, Rust), records it in the edit zone (a small state file in
    the system temp dir, keyed by repo).
  - On a `Read` of such a file, skeletonizes functions that are more than
    `edit_zone_hops` (1) graph hops from the edit zone — full fidelity for the
    code you're working near, a signature + effect-signature + docstring
    contract for the distant context — and attaches sanctioned-channel
    steering (`additionalContext`).
  - **Edit-invertibility guarantee:** a read is *only ever* remapped by that
    zoned skeletonization of graph-distant code (which you're editing
    elsewhere, so you won't string-edit against it). Every other read is served
    **verbatim** — no strip, no skeleton — so an `Edit` can always match the
    real file. A **re-read** is served verbatim too (the agent came back to it,
    likely to edit): this self-corrects a first-read edit that missed against a
    skeleton (fail → re-read → verbatim → succeeds). Set
    `SLOP_COMPRESS_READS=0` to disable read compression entirely (steering
    still applies).
  - Non-`Read`/`Write`/`Edit` calls, and reads of files in no supported
    language, pass through untouched.

  > Cost: a read that triggers zoned compression builds the graph
  > (~0.1–0.5s depending on repo size, needs `<repo>/index.scip`). Small
  > files, cold reads, and re-reads skip it. Preview/measure with `slop
  > compress <repo> <file> --edit-file <other-file> --hops 1 --stats`.
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

Register in `.claude/settings.json` (or just run `slop install`, which writes
exactly this). The matcher covers the write tools too, so the hook can record
the session edit zone that read compression measures graph distance against:

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
    "PostToolUse": [
      {
        "matcher": "Read|Write|Edit|MultiEdit",
        "hooks": [
          { "type": "command", "command": "/abs/path/to/target/release/slop hook post-tool-use" }
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

> Field-name note: the read-remap reads the file text from `tool_output` or
> `tool_response` (string, or `{content|file|text}` object). If your Claude
> Code version names it differently, the hook passes the read through
> unchanged rather than corrupting it. Steering still applies.

---

## 3. Gate

`slop gate <repo>` is the validation gate for a CI step or an agent fix-loop.
It runs the full check and **exits non-zero when any finding is at or above the
`--fail-on` threshold** (default: Blocking), printing machine-readable JSON
(findings + `fix_guidance`) the driving agent loops on. slop reports; the agent
edits.

```
slop gate <repo> [--base HEAD] [--all] [--tier3] [--fail-on blocking|warning|advisory] [--reindex] [--worktree]
```

- `--fail-on warning` makes the gate fail on Warnings too — this is what lets
  the loop act on `duplicate-exact`, `complexity-spike`, and `purity-lie`, not
  just the deterministic blockers (`infra-bypass`, `circular-import`). The JSON
  reports `fail_on`, `failing` (count at/above threshold), and `blocking`.
- `--reindex` regenerates the SCIP index (via `scip-python`) before checking,
  so a re-run after edits sees the new graph.
- `--worktree` runs inside an isolated `git worktree` of the repo.

Agent fix-loop shape (driven by the host, not slop):

```
loop:
  slop gate <repo> --reindex   # exits 0 → done
  → feed the JSON findings back to the agent
  → agent edits
```

---

## 4. Proxy

`slop proxy` is an `ANTHROPIC_BASE_URL` reverse proxy for agent-agnostic
steering and token observability (never cert-MITM). Point any Anthropic client
at it:

```
slop proxy --port 8787 [--repo <repo>] [--steer] [--log slop-proxy.jsonl]
ANTHROPIC_BASE_URL=http://localhost:8787 <your agent>
```

- Streams responses through untouched (SSE `stream:true` included).
- Appends one JSONL record per request to `--log` with model + token usage.
- `--steer` (with `--repo`) augments the request's system prompt with the repo's
  **world model** — the same sanctioned channels, layer rules and capability
  index the `SessionStart` hook injects (`world::render`). The hook only reaches
  Claude Code; this is how every other Anthropic client gets it. Rendered once
  at startup, so it costs one graph build per launch rather than one per
  request, and degrades to policy-only when the repo has no index.
- Compression stays on the hook path. The proxy sees the request after context
  is assembled, which is too late to compress it (D10).
