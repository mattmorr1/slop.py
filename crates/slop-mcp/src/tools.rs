//! The additive MCP tools (M4c, D9): `find_capability`, `validate_change`,
//! `get_context_envelope`, `query_subgraph`. Each is a thin adapter that
//! runs the *same* analysis the CLI runs (via `slop_analyze::check`) and
//! serializes the result — never a reimplementation.

use std::path::PathBuf;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use slop_analyze::check::{self, CheckRequest};
use slop_analyze::context::ContextRequest;
use slop_analyze::envelope::DEFAULT_MIN_PROBABILITY_PPM;
use slop_analyze::refresh::Refresher;
use slop_analyze::query;
use slop_analyze::search;
use slop_graph::{EdgeKind, Effect};

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
            "description": "Find what this codebase ALREADY provides for a described intent, before writing a new implementation. Ranked by name/module/docstring match and how many places call it. This is the entry point to the other graph tools: every id it returns can be fed straight to `query_subgraph` or `get_context_envelope`. Reimplementing something that exists is the most common slop class — ask here first.",
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
            "name": "validate_change",
            "description": "Run slop's full detector suite over a repo and return findings (blocking/warning/advisory) with machine-readable fix guidance. Defaults to judging the working-tree diff vs a git ref; set all=true for the whole repo. This is the same analysis `slop check` runs.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": {"type": "string", "description": "Repo root (defaults to the server's launch repo)"},
                    "base": {"type": "string", "description": "Git ref to diff against (default HEAD)"},
                    "all": {"type": "boolean", "description": "Judge the whole repo instead of the diff"},
                    "tier3": {"type": "boolean", "description": "Also run advisory semantic-redundancy judging (needs ANTHROPIC_API_KEY)"}
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
            "name": "get_context_envelope",
            "description": "Build the context envelope around a target entity: the code most likely needed when editing it, scored by calibrated relevance, full-fidelity inside the edit zone and skeletonized beyond, capped by a token budget. Returns items with source text and per-feature reasons.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": {"type": "string", "description": "Repo root (defaults to the server's launch repo)"},
                    "target_entity": {"type": "string", "description": "Entity id, e.g. `billing.gateways::StripeGateway::charge`"},
                    "token_budget": {"type": "integer", "description": "Rough token budget for context beyond the target (default 8000)"},
                    "edit_zone_hops": {"type": "integer", "description": "BFS hops from the target that stay full-fidelity (default 1)"},
                    "min_probability": {"type": "number", "description": "Calibrated relevance an item needs to be shown, 0-1 (default 0.01; 0 fills the budget, higher is terser)"}
                },
                "required": ["target_entity"]
            }
        },
        {
            "name": "expand_entity",
            "description": "Full verbatim source of one entity from the same repository snapshot a context envelope came from: the inverse of a skeleton. Use it when a skeletonized item turns out to matter. `state` other than `resolved` means its line range came from an index built from different content.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": {"type": "string", "description": "Repo root (defaults to the server's launch repo)"},
                    "entity": {"type": "string", "description": "Entity id from an envelope item"}
                },
                "required": ["entity"]
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
        }
    ])
}

/// Dispatch a `tools/call`. Returns the text payload for the single
/// `content` item; the transport wraps it in the MCP envelope.
pub fn call(ctx: &ToolCtx, name: &str, args: &Value) -> Result<String> {
    match name {
        "find_capability" => find_capability(ctx, args),
        "validate_change" => validate_change(ctx, args),
        "assess_write" => assess_write(ctx, args),
        "get_context_envelope" => get_context_envelope(ctx, args),
        "query_subgraph" => query_subgraph(ctx, args),
        "expand_entity" => expand_entity(ctx, args),
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

fn validate_change(ctx: &ToolCtx, args: &Value) -> Result<String> {
    let repo = ctx.repo(args);
    let result = check::run(CheckRequest {
        repo,
        index: ctx.index(args),
        policy: None,
        all: args.get("all").and_then(Value::as_bool).unwrap_or(false),
        tier3: args.get("tier3").and_then(Value::as_bool).unwrap_or(false),
        base: args
            .get("base")
            .and_then(Value::as_str)
            .unwrap_or("HEAD")
            .to_string(),
    })?;
    Ok(serde_json::to_string_pretty(&json!({
        "schema_version": result.schema_version,
        "snapshot": result.snapshot,
        "freshness": result.freshness,
        "scope": result.scope,
        "coverage": result.coverage,
        "findings": result.findings,
        "blocking": result.blocking,
        "health": result.health_line,
        "current_health": result.current_health,
        "policy_is_empty": result.policy_is_empty,
    }))?)
}

fn get_context_envelope(ctx: &ToolCtx, args: &Value) -> Result<String> {
    let repo = ctx.repo(args);
    let target = args
        .get("target_entity")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("target_entity is required"))?;
    let analysis = ctx.analysis(&repo, args)?;
    let artifact = analysis.context(ContextRequest {
        target_entity: target,
        token_budget: args.get("token_budget").and_then(Value::as_u64).unwrap_or(8000) as usize,
        edit_zone_hops: args.get("edit_zone_hops").and_then(Value::as_u64).unwrap_or(1) as usize,
        selection: Default::default(),
        min_probability_ppm: match args.get("min_probability").and_then(Value::as_f64) {
            Some(p) if (0.0..=1.0).contains(&p) => (p * 1_000_000.0).round() as u32,
            Some(p) => bail!("min_probability must be between 0 and 1, got {p}"),
            None => DEFAULT_MIN_PROBABILITY_PPM,
        },
    });
    Ok(serde_json::to_string_pretty(&artifact)?)
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

fn expand_entity(ctx: &ToolCtx, args: &Value) -> Result<String> {
    let repo = ctx.repo(args);
    let entity = args
        .get("entity")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("entity is required"))?;
    let analysis = ctx.analysis(&repo, args)?;
    match analysis.expand(entity) {
        Some(expansion) => Ok(serde_json::to_string_pretty(&expansion)?),
        None => Ok(serde_json::to_string_pretty(&json!({
            "entity": entity,
            "snapshot": analysis.id(),
            "note": "entity not found in this snapshot",
        }))?),
    }
}
