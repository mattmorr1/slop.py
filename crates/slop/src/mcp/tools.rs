//! The MCP tools (M4c, D9): `find_capability`, `assess_write`, `query_subgraph`, `reconcile`,
//! `acknowledge`. Each is a thin adapter that
//! runs the *same* analysis the CLI runs (via `slop_core::analyze::check`) and
//! serializes the result — never a reimplementation.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use slop_core::analyze::check;
use slop_core::analyze::prewrite;
use slop_core::analyze::reconcile as gate;
use slop_core::analyze::session::{self, Session};
use slop_core::analyze::refresh::Refresher;
use slop_core::analyze::query;
use slop_core::analyze::search;
use slop_core::graph::{EdgeKind, Effect};

/// The server's launch context: the repo (and optional index) the tools
/// default to when a call omits them.
pub struct ToolCtx {
    pub default_repo: PathBuf,
    pub default_index: Option<PathBuf>,
    /// Background reindex for the default repository; None in tests and one-shot use.
    pub refresher: Option<Refresher>,
}

impl ToolCtx {
    fn repo(&self, args: &Value) -> PathBuf {
        args.get("repo")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| self.default_repo.clone())
    }

    fn index(&self, args: &Value) -> Option<PathBuf> {
        args.get("index")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .or_else(|| self.default_index.clone())
    }

    /// The snapshot for `repo`; edits the index has not seen queue a background reindex.
    fn analysis(&self, repo: &std::path::Path, args: &Value) -> Result<std::sync::Arc<check::Analysis>> {
        let analysis = check::load_analysis(repo, self.index(args).as_deref())?;
        if let Some(refresher) = self.refresher.as_ref().filter(|_| repo == self.default_repo) {
            refresher.request_if_edited(&analysis);
        }
        Ok(analysis)
    }
}

/// The `tools/list` payload. Kept as data (not derived) so the JSON Schemas
/// stay readable next to the handlers that consume them.
pub fn definitions() -> Value {
    json!([
        {
            "name": "find_capability",
            "description": "Find what this codebase ALREADY provides for a described intent, before writing a new implementation. Ranked by name/module/docstring match and how many places call it. This is the entry point to the other graph tools: every id it returns can be fed straight to `query_subgraph`. Reimplementing something that exists is the most common slop class — ask here first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": {"type": "string", "description": "Repo root (defaults to the server's launch repo)"},
                    "intent": {"type": "string", "description": "What you are about to write, in words, e.g. `parse an ISO date` or `send an HTTP request with retries`"},
                    "effect": {
                        "type": "string",
                        "enum": ["net", "fs_read", "fs_write", "db", "env", "throws", "nondeterminism", "state", "concurrency", "unknown"],
                        "description": "Restrict to functions carrying this effect — with an empty intent this answers `what owns the network here`"
                    },
                    "limit": {"type": "integer", "description": "Max results (default 10)"}
                }
            }
        },
        {
            "name": "assess_write",
            "description": "Assess proposed source before writing it. Returns direct sanctioned-channel bypasses and existing functions whose distinctive callee neighborhood suggests the code belongs there instead.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": {"type": "string", "description": "Repo root (defaults to the server's launch repo)"},
                    "file": {"type": "string", "description": "Repo-relative destination path; selects the language"},
                    "content": {"type": "string", "description": "Complete proposed file content"},
                    "limit": {"type": "integer", "description": "Maximum reuse suggestions (default 3)"}
                },
                "required": ["file", "content"]
            }
        },
        {
            "name": "query_subgraph",
            "description": "Explore the effect graph around an entity: its callers/callees/imports out to a depth, each neighbor's relation and distance, plus the target's inferred effect signature. Use this to learn how a codebase routes an effect (e.g. what owns Net) before writing code.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": {"type": "string", "description": "Repo root (defaults to the server's launch repo)"},
                    "entity": {"type": "string", "description": "Entity id to center on"},
                    "depth": {"type": "integer", "description": "BFS hops to traverse (default 1)"},
                    "edge_kinds": {
                        "type": "array",
                        "items": {"type": "string", "enum": ["Contains", "Calls", "Imports", "Inherits", "HasEffect"]},
                        "description": "Restrict to these edge kinds (default: all)"
                    }
                },
                "required": ["entity"]
            }
        },
        {
            "name": "reconcile",
            "description": "Check what you changed before finishing. Returns provable problems (an E-sound copy of an existing function, a call to a name that does not exist, a sanctioned-channel bypass), each with an id and an action, plus ranked advisory matches and slop's full detector findings. With session_id it judges only what this session changed; otherwise the working-tree diff against `base`. The Stop hook blocks on the same provable problems.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": {"type": "string", "description": "Repo root (defaults to the server's launch repo)"},
                    "session_id": {"type": "string", "description": "Claude Code session id, given at session start; scopes the check to this session's changes"},
                    "base": {"type": "string", "description": "Git ref to diff against without a session (default HEAD)"}
                }
            }
        },
        {
            "name": "acknowledge",
            "description": "Record that a provable problem reported by `reconcile` or the Stop hook is intentional, so it no longer blocks. Appends the id and your reason to .slop/acks.jsonl, where reviewers see it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": {"type": "string", "description": "Repo root (defaults to the server's launch repo)"},
                    "id": {"type": "string", "description": "The finding id, e.g. `duplicate:3f2a9c0d1e4b5a6f`"},
                    "reason": {"type": "string", "description": "Why the finding is intentional"},
                    "session_id": {"type": "string", "description": "Claude Code session id, recorded with the acknowledgement"}
                },
                "required": ["id", "reason"]
            }
        }
    ])
}

/// Dispatch a `tools/call`. Returns the text payload for the single
/// `content` item; the transport wraps it in the MCP envelope.
pub fn call(ctx: &ToolCtx, name: &str, args: &Value) -> Result<String> {
    match name {
        "find_capability" => find_capability(ctx, args),
        "assess_write" => assess_write(ctx, args),
        "query_subgraph" => query_subgraph(ctx, args),
        "reconcile" => reconcile(ctx, args),
        "acknowledge" => acknowledge(ctx, args),
        other => Err(anyhow!("unknown tool: {other}")),
    }
}

fn assess_write(ctx: &ToolCtx, args: &Value) -> Result<String> {
    let repo = ctx.repo(args);
    let file = args
        .get("file")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("file is required"))?;
    let content = args
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("content is required"))?;
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(3) as usize;
    let analysis = ctx.analysis(&repo, args)?;
    Ok(serde_json::to_string_pretty(
        &analysis.assess_write(file, content, limit)?,
    )?)
}

/// `slop.toml`'s effect names, so what an agent passes here matches what it
/// reads in the policy and the world model.
fn parse_effect(name: &str) -> Option<Effect> {
    Some(match name {
        "net" => Effect::Net,
        "fs_read" => Effect::FsRead,
        "fs_write" => Effect::FsWrite,
        "db" => Effect::Db,
        "env" => Effect::Env,
        "throws" => Effect::Throws,
        "nondeterminism" => Effect::Nondeterminism,
        "state" => Effect::StateMutate,
        "concurrency" => Effect::Concurrency,
        "unknown" => Effect::Unknown,
        _ => return None,
    })
}

fn find_capability(ctx: &ToolCtx, args: &Value) -> Result<String> {
    let repo = ctx.repo(args);
    let intent = args.get("intent").and_then(Value::as_str).unwrap_or("");
    let effect = args.get("effect").and_then(Value::as_str).and_then(parse_effect);
    if intent.trim().is_empty() && effect.is_none() {
        return Err(anyhow!("give an `intent` to search for, an `effect` to filter by, or both"));
    }
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize;
    let analysis = ctx.analysis(&repo, args)?;
    let found = search::find(&analysis.built, intent, effect, limit);
    if found.is_empty() {
        return Ok(serde_json::to_string_pretty(&json!({
            "intent": intent,
            "capabilities": [],
            "note": "nothing in this codebase matches — writing it appears to be justified",
        }))?);
    }
    Ok(serde_json::to_string_pretty(&json!({
        "intent": intent,
        "capabilities": found,
    }))?)
}

fn reconcile(ctx: &ToolCtx, args: &Value) -> Result<String> {
    let repo = ctx.repo(args);
    let session_id = args.get("session_id").and_then(Value::as_str);
    let session = match session_id {
        Some(id) => session::load(&repo, id)?,
        None => Session { head: Some(args.get("base").and_then(Value::as_str).unwrap_or("HEAD").to_string()), ..Session::default() },
    };
    let changes = session::changes(&repo, &session)?;
    // One capture that reindexes in the foreground, never beside a background refresh.
    let analysis = {
        let _exclusive = ctx.refresher.as_ref().filter(|_| repo == ctx.default_repo).map(Refresher::exclusive);
        check::load_analysis_fresh(&repo, ctx.index(args).as_deref(), check::Freshness::Reindex)?
    };
    let sidecar = prewrite::load_sidecar(&repo)?;
    let report = gate::reconcile(&repo, &changes, sidecar.as_ref(), &analysis.policy);
    let acked = gate::acknowledged(&repo)?;
    let (acknowledged, open): (Vec<_>, Vec<_>) = report.findings.into_iter().partition(|f| acked.contains(&f.id));
    let result = check::report(&analysis, false, session.head.clone().unwrap_or_else(|| "HEAD".to_string()))?;
    Ok(serde_json::to_string_pretty(&json!({
        "scope": if session_id.is_some() { "session" } else { "diff" },
        "changed_files": changes.iter().map(|c| &c.file).collect::<Vec<_>>(),
        "provable": open.iter().map(|f| json!({ "id": f.id, "file": f.file, "line": f.line + 1, "fact": f.fact, "action": f.action })).collect::<Vec<_>>(),
        "acknowledged": acknowledged.iter().map(|f| &f.id).collect::<Vec<_>>(),
        "advisory": report.advisory,
        "sidecar": if sidecar.is_some() { "loaded" } else { "absent: duplicates and suggestions need SessionStart or `slop index` to write it" },
        "detectors": {
            "snapshot": result.snapshot,
            "freshness": result.freshness,
            "coverage": result.coverage,
            "findings": result.findings,
            "blocking": result.blocking,
            "health": result.health_line,
        },
    }))?)
}

fn acknowledge(ctx: &ToolCtx, args: &Value) -> Result<String> {
    let text = |key: &str| args.get(key).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty()).ok_or_else(|| anyhow!("{key} is required"));
    let (id, reason) = (text("id")?, text("reason")?);
    gate::acknowledge(&ctx.repo(args), args.get("session_id").and_then(Value::as_str), id, reason)?;
    Ok(serde_json::to_string_pretty(&json!({ "acknowledged": id, "reason": reason }))?)
}

fn parse_edge_kinds(args: &Value) -> Option<Vec<EdgeKind>> {
    let arr = args.get("edge_kinds")?.as_array()?;
    let kinds = arr
        .iter()
        .filter_map(Value::as_str)
        .filter_map(|s| match s {
            "Contains" => Some(EdgeKind::Contains),
            "Calls" => Some(EdgeKind::Calls),
            "Imports" => Some(EdgeKind::Imports),
            "Inherits" => Some(EdgeKind::Inherits),
            "HasEffect" => Some(EdgeKind::HasEffect),
            _ => None,
        })
        .collect();
    Some(kinds)
}

fn query_subgraph(ctx: &ToolCtx, args: &Value) -> Result<String> {
    let repo = ctx.repo(args);
    let entity = args
        .get("entity")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("entity is required"))?;
    let depth = args.get("depth").and_then(Value::as_u64).unwrap_or(1) as usize;
    let analysis = ctx.analysis(&repo, args)?;
    let kinds = parse_edge_kinds(args);
    match query::subgraph(&analysis.built, entity, depth, kinds.as_deref()) {
        Some(sg) => Ok(serde_json::to_string_pretty(&sg)?),
        None => Ok(serde_json::to_string_pretty(&json!({
            "entity": entity,
            "note": "entity not found in the graph",
        }))?),
    }
}
