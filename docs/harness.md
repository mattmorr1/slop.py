# The slop harness (v3)

`slop` never edits code. The agent host makes every change; slop feeds it through
three surfaces:

1. **Hooks**: the session baseline and world model at start, warn-only steering
   before each write, and the reconcile gate at Stop.
2. **MCP tools**: `find_capability`, `assess_write`, `query_subgraph`,
   `reconcile`, `acknowledge`.
3. **Gate**: `slop gate`, the CI and fix-loop entry point.

All of it reuses the engine `slop check` runs.

## Install

**Claude Code** gets all three from the plugin in this repository:

```
/plugin marketplace add mattmorr1/slop.py
/plugin install slop@slop
```

The plugin (`plugin/`) carries the manifest with the MCP server, `hooks/hooks.json`
(SessionStart; PreToolUse on `Write|Edit|MultiEdit`; Stop; explicit timeouts),
the skill, and `bin/slop-run`. The launcher finds the binary once per plugin
version and caches its absolute path in `${TMPDIR:-/tmp}/slop-plugin-<version>.path`:

1. `$SLOP_BIN`, always, and never cached;
2. `slop` on `PATH` when `slop --version` matches the plugin version;
3. `uvx --from slop-cli==<version> slop`, resolved to a real path once.

With none of them it exits 1 with a message naming `SLOP_BIN`, which Claude Code
shows as a non-blocking hook error. `slop setup --ai claude` now only removes the
`.mcp.json` entry, hooks and project skill an older setup wrote, and prints the
two commands above.

**Codex**: `slop setup <repo> --ai codex` adds a delimited MCP block to
`.codex/config.toml`. Codex has no hooks. `slop uninstall` removes only what
setup wrote; `slop doctor` checks the host, receipt and index.

`[agent] gate` in `slop.toml` sets how hard the hooks push:

| `gate` | SessionStart context | PreToolUse steering | Stop gate |
| --- | --- | --- | --- |
| `off` | no | no | no |
| `steer` | yes | yes | no |
| `reconcile` (default) | yes | yes | yes |

---

## 1. Hooks

PreToolUse and Stop never build the graph or read SCIP. They read `slop.toml`, the
session record and the prewrite sidecar, a schema-versioned JSON file in the
system temp directory written by SessionStart or `slop index`. An internal error
fails open (the write or the stop proceeds) and is logged to stderr and
`.slop/hooks.jsonl`; nothing is swallowed.

### SessionStart (and SubagentStart)

- Records the **session baseline** in `.slop/session/<session_id>.json`: HEAD,
  plus a blake3 hash and a copy of every source file already dirty or untracked.
  The Stop gate judges only what differs from that baseline, never the user's
  pre-existing work.
- Builds the graph once and writes the sidecar: the callee neighborhood, the
  E-sound and structural hash indexes, and the defined-name table used for
  suggestions. A stale index is not persisted; the error is logged.
- Injects the **world model**: sanctioned channels, layer rules and a capability
  index of the most-referenced functions by effect, budgeted to 7000 chars, then a
  pointer to `reconcile` carrying the session id.

### PreToolUse (`Write|Edit|MultiEdit`)

Judges the content a write is about to produce. An `Edit` is applied to the file
on disk first, so the check sees the result rather than a fragment. It is
**warn-only**: it never sets `permissionDecision`.

- **Provable duplicates**: a new or changed function whose E-sound hash matches an
  existing function, named first with its location. A pure rename is silent.
- **Channel bypass**: direct effect acquisition outside the sanctioned channel.
  Silent when the repo has no channels (D8).
- **Missing names**: `slop: missing ...` for each call to a name that provably
  does not exist and that the file on disk did not already make, with up to three
  "did you mean" suggestions (rules below).
- **Reuse with usage**: existing functions with the same distinctive callees. The
  top two carry their body and one real call site, because models reuse what they
  see used, not what they see named.

### Stop: the reconcile gate

Stop compares every changed file with its baseline and reports **provable**
problems only:

| id | fact |
| --- | --- |
| `duplicate:<hash>` | a new function is E-sound equivalent to an existing one |
| `missing:<file>:<name>` | a new call names something that does not exist |
| `infra-bypass:<file>:<call>` | new direct acquisition of an effect outside its channel |

If any remain, Stop returns `{"decision": "block", "reason": ...}`. The reason
lists up to 8 findings, each with its id, `file:line`, the fact and an action,
then ranked matches as advice that never blocks (same shape, overlapping calls),
and ends with "If this is intentional, call the slop `acknowledge` tool with the
id and a reason."

It cannot loop: Stop allows when `stop_hook_active` is set, and drops ids that are
acknowledged or already blocked once this session.

An existing function counts as the original only if it held that name and hash
both at the baseline and now. A renamed original, or the new copy indexed by a
sidecar rebuilt mid-session, never matches.

E-sound equivalence keeps a leading docstring, so a copy with a different
docstring is not provably equivalent. It surfaces as advice instead.

### `missing`: rules (Python)

A call is reported only when the name provably does not exist. For a bare call
`f(...)`, `f` is bound nowhere in the file (any binding in any scope counts:
assignment, parameter, import, def, class, comprehension target, `global`), and
is not a builtin or dunder.

For `m.f(...)` or `C.f(...)`, the receiver `m` is bound exactly once, by an import
of a repository module or by a closed class (no bases, keywords or decorators).
Then:

- The module path must resolve to exactly one file. Candidate roots are the
  importing file's package root, the repo root and `src/`; a package's
  `__init__.py` wins over a same-named module.
- `f` must be absent from the module's top-level names (including names assigned
  through `global`), and no child module or directory may be named `f`.

A class's names include any attribute assigned anywhere in its file
(`C.f = ...`). The whole file is skipped on a star import, a module-level
`__getattr__`, any use of `__builtins__`, or a call to `exec`, `eval`,
`globals` or `vars`.
Attribute calls on anything else (`obj.method()`) are never judged, because types
are unknown.

Measured on flask, httpx, requests, click and Vigil-SOC/vigil (690 non-test
files): 13 skipped, 0 findings. Suggestions are BM25 over the sidecar's defined
names, each with the file that defines it.

Every hook call appends one line to `.slop/hooks.jsonl`: event, file, latency,
injected tokens, duplicates, reuse, missing, and the Stop decision and block
count. `SLOP_HOOK_LOG=0` turns it off. `post-tool-use` and `user-prompt-submit`
remain as no-op events, so hooks left by an older setup still exit cleanly.

---

## 2. MCP tools

Newline-delimited JSON-RPC 2.0 over stdio: `slop mcp <repo-root> [--index path]`.
The plugin registers it; without the plugin, add it to `.mcp.json`:

```json
{ "mcpServers": { "slop": { "type": "stdio", "command": "/abs/path/to/slop", "args": ["mcp", "/abs/path/to/repo"] } } }
```

| Tool | Purpose | Key args |
| --- | --- | --- |
| `find_capability` | What the codebase already provides for an intent | `intent`, `effect`, `limit` |
| `assess_write` | Proposed-source policy findings and existing homes | `file`, `content`, `limit` |
| `query_subgraph` | Callers, callees, imports and the effect signature | `entity`, `depth`, `edge_kinds` |
| `reconcile` | The Stop gate's provable findings, advice and the full detector suite | `session_id` or `base` |
| `acknowledge` | Mark a provable finding intentional | `id`, `reason`, `session_id` |

- `reconcile` with `session_id` judges what the session changed; without it, the
  working-tree diff against `base` (default HEAD). It lists `provable` findings,
  `acknowledged` ids, `advisory` matches and `detectors` (the `slop check` result
  for the same diff). It needs the sidecar for duplicates and suggestions and
  says when it is absent.
- `acknowledge` appends `{ts, session_id, id, reason}` to `.slop/acks.jsonl`.
  Acknowledgements are repository-wide and permanent; commit the file so
  reviewers see the reasons.
- `find_capability` is the intent-first entry point; feed any id it returns to
  `query_subgraph`. Ranking is deterministic: name, module and docstring token
  overlap, ties broken by caller count.

---

## 3. Gate

`slop gate <repo>` runs the full check and **exits non-zero when any finding is at
or above `--fail-on`** (default Blocking), printing JSON findings with
`fix_guidance` for the driving agent.

```
slop gate <repo> [--base HEAD] [--all] [--fail-on blocking|warning|advisory] [--reindex] [--worktree]
```

- `--fail-on warning` also fails on `duplicate-exact`, `duplicate-equivalent` and
  `purity-lie`. The JSON reports `fail_on`, `failing` and `blocking`.
- `--reindex` regenerates every SCIP artifact first; `--worktree` runs in an
  isolated `git worktree`.
