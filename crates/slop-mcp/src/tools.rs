//! The three additive MCP tools (M4c, D9): `validate_change`,
//! `get_context_envelope`, `query_subgraph`. Each is a thin adapter that
//! runs the *same* analysis the CLI runs (via `slop_analyze::check`) and
//! serializes the result — never a reimplementation.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use slop_analyze::check::{self, CheckRequest};
use slop_analyze::envelope::{self, EnvelopeConfig};
use slop_analyze::query;
use slop_graph::EdgeKind;

/// The server's launch context: the repo (and optional index) the tools
/// default to when a call omits them.
pub struct ToolCtx {
    pub default_repo: PathBuf,
    pub default_index: Option<PathBuf>,
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
}

/// The `tools/list` payload. Kept as data (not derived) so the JSON Schemas
/// stay readable next to the handlers that consume them.
pub fn definitions() -> Value {
    json!([
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
            "name": "get_context_envelope",
            "description": "Build the effect-typed context envelope around a target entity: the most relevant code to see when editing it, full-fidelity inside the edit zone and skeletonized beyond, packed under a token budget. Returns ranked items with source text.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": {"type": "string", "description": "Repo root (defaults to the server's launch repo)"},
                    "target_entity": {"type": "string", "description": "Entity id, e.g. `billing.gateways::StripeGateway::charge`"},
                    "token_budget": {"type": "integer", "description": "Rough token budget for context beyond the target (default 8000)"},
                    "edit_zone_hops": {"type": "integer", "description": "BFS hops from the target that stay full-fidelity (default 1)"}
                },
                "required": ["target_entity"]
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
        "validate_change" => validate_change(ctx, args),
        "get_context_envelope" => get_context_envelope(ctx, args),
        "query_subgraph" => query_subgraph(ctx, args),
        other => Err(anyhow!("unknown tool: {other}")),
    }
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
        "findings": result.findings,
        "blocking": result.blocking,
        "health": result.health_line,
        "policy_is_empty": result.policy_is_empty,
    }))?)
}

fn get_context_envelope(ctx: &ToolCtx, args: &Value) -> Result<String> {
    let repo = ctx.repo(args);
    let target = args
        .get("target_entity")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("target_entity is required"))?;
    let analysis = check::load_analysis(&repo, ctx.index(args).as_deref())?;
    let mut config = EnvelopeConfig::default();
    if let Some(b) = args.get("token_budget").and_then(Value::as_u64) {
        config.token_budget = b as usize;
    }
    if let Some(h) = args.get("edit_zone_hops").and_then(Value::as_u64) {
        config.edit_zone_hops = h as usize;
    }
    let items = envelope::build_envelope(&analysis.built, &analysis.facts, &repo, target, &config);
    if items.is_empty() {
        return Ok(serde_json::to_string_pretty(&json!({
            "target": target,
            "items": [],
            "note": "target not found in the graph, or no relevant context under the budget",
        }))?);
    }
    Ok(serde_json::to_string_pretty(&json!({
        "target": target,
        "items": items,
    }))?)
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
    let analysis = check::load_analysis(&repo, ctx.index(args).as_deref())?;
    let kinds = parse_edge_kinds(args);
    match query::subgraph(&analysis.built, entity, depth, kinds.as_deref()) {
        Some(sg) => Ok(serde_json::to_string_pretty(&sg)?),
        None => Ok(serde_json::to_string_pretty(&json!({
            "entity": entity,
            "note": "entity not found in the graph",
        }))?),
    }
}
