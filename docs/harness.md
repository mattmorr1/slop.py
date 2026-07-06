# The slop harness (v2)

`slop` never drives edits itself (D9). It exposes three surfaces an existing
agent host (e.g. Claude Code) drives:

1. **MCP tools** — `validate_change`, `get_context_envelope`, `query_subgraph`.
2. **Hooks** — read-path compression + sanctioned-channel steering.
3. **Gate** — `slop gate`, the CI/loop entry point (see below).
4. **Proxy** — `slop proxy`, agent-agnostic steering/observability (see below).

All of it reuses the same analysis engine `slop check` runs — no reimplementation.

Build the binary first: `cargo build --release` → `target/release/slop`.

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
| `validate_change` | Full detector suite → findings + fix guidance | `all`, `base`, `tier3` |
| `get_context_envelope` | Effect-typed, budget-packed context around an entity | `target_entity`, `token_budget`, `edit_zone_hops` |
| `query_subgraph` | Callers/callees/imports + effect signature | `entity`, `depth`, `edge_kinds` |

---

## 2. Hooks

Both hooks derive steering from `slop.toml` alone (no SCIP index, no graph
build) so they are cheap enough to run on every read/prompt.

- `slop hook post-tool-use` — maintains a **session edit zone** and does
  **zoned graph-distance compression** on reads:
  - On a `Write`/`Edit`/`MultiEdit` of a `.py` file, records it in the edit
    zone (a small state file in the system temp dir, keyed by repo).
  - On a `Read` of a `.py` file, skeletonizes functions that are more than
    `edit_zone_hops` (1) graph hops from the edit zone — full fidelity for the
    code you're working near, a signature + effect-signature + docstring
    contract for everything else — and attaches sanctioned-channel steering
    (`additionalContext`). Falls back to a plain comment/blank strip on a cold
    read (empty edit zone), a small file, or when there's no SCIP index.
  - Non-`Read`/`Write`/`Edit`, non-`.py` reads pass through untouched.

  > Cost: a read that triggers zoned compression builds the graph
  > (~0.1–0.5s depending on repo size, needs `<repo>/index.scip`). Small
  > files and cold reads skip it. Preview/measure with `slop compress`:
  > `slop compress <repo> <file> --edit <entity-id> --hops 1 --stats`.
- `slop hook user-prompt-submit` — injects the repo's sanctioned-channel
  policy plus a pointer to `validate_change` as pre-hoc steering.

Register in `.claude/settings.json`:

```json
{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Read",
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
It runs the full check and **exits non-zero when any finding is Blocking**,
printing machine-readable JSON (findings + `fix_guidance`) the driving agent
loops on. slop reports; the agent edits.

```
slop gate <repo> [--base HEAD] [--all] [--tier3] [--reindex] [--worktree]
```

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
- `--steer` (with `--repo`) augments the request's system prompt with the
  repo's sanctioned-channel policy.
