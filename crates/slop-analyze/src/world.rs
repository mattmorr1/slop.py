//! Session world model (W4): the codebase facts an agent should have *before*
//! it designs anything, injected once at `SessionStart` and into every
//! `SubagentStart`.
//!
//! The read-path remap steers after a read and the pre-check steers at a write;
//! both are corrections applied to a decision already made. This fires up front
//! and answers the question that prevents the most common slop class — what does
//! this codebase already have, and what is it meant to route through? Subagents
//! otherwise start with no codebase context at all.
//!
//! Policy facts (channels, layers) are free. The capability index needs a graph,
//! so it is best-effort: with no SCIP index the model degrades to policy-only
//! rather than failing. Written as statements, not instructions — that is what
//! `additionalContext` is for.

use std::collections::HashMap;

use slop_graph::{Effect, NodeType};

use crate::build::BuiltGraph;
use crate::policy::Policy;
use crate::source::{is_test_entity, is_test_file};

/// `additionalContext` is capped at 10k chars by the agent host; stay under it
/// with headroom so a long capability list never truncates mid-line.
pub const DEFAULT_BUDGET: usize = 7000;

/// Environment variables to name before the list stops earning its space.
const ENV_LIMIT: usize = 12;

/// How many capabilities to list per effect group. Enough to show the pattern,
/// not so many that the model reads as a directory listing.
const PER_GROUP: usize = 6;

/// Lattice effect as the name `slop.toml` uses, so what an agent reads here
/// matches what it would write in a policy.
fn effect_label(effect: Effect) -> &'static str {
    match effect {
        Effect::Net => "net",
        Effect::FsRead => "fs_read",
        Effect::FsWrite => "fs_write",
        Effect::Db => "db",
        Effect::Env => "env",
        Effect::Throws => "throws",
        Effect::Nondeterminism => "nondeterminism",
        Effect::StateMutate => "state",
        Effect::Concurrency => "concurrency",
        Effect::Unknown => "unknown",
    }
}

/// Sanctioned channels as fact lines, or `None` with no policy (D8).
fn channel_lines(policy: &Policy) -> Option<Vec<String>> {
    if policy.channels.is_empty() {
        return None;
    }
    let mut keys: Vec<&String> = policy.channels.keys().collect();
    keys.sort();
    let lines: Vec<String> = keys
        .iter()
        .filter(|k| !policy.channels[**k].is_empty())
        .map(|k| format!("  {}: {}", k, policy.channels[*k].join(", ")))
        .collect();
    (!lines.is_empty()).then_some(lines)
}

/// Architectural layer rules as fact lines.
fn layer_lines(policy: &Policy) -> Vec<String> {
    policy
        .layers
        .iter()
        .filter(|l| !l.matches.is_empty() && !l.forbid.is_empty())
        .map(|l| {
            format!(
                "  {} ({}) does not perform: {}",
                l.name,
                l.matches.join(", "),
                l.forbid.join(", ")
            )
        })
        .collect()
}

/// The codebase's reused building blocks, grouped by what they do: internal
/// callables ranked by how many places reference them. Reimplementing one of
/// these is the redundancy slop class, so naming them up front is the cheapest
/// prevention available.
fn capability_lines(built: &BuiltGraph) -> Vec<String> {
    let refs = crate::search::reference_counts(built);

    // Group by effect signature: the pure helpers are as reusable as the
    // effectful ones, so they get their own bucket rather than being dropped.
    let mut groups: HashMap<&'static str, Vec<(usize, String)>> = HashMap::new();
    for (idx, entity) in built.graph.entities() {
        // Test helpers are heavily referenced *by tests*, so they rank high while
        // being the last thing an agent should reuse in production code.
        if entity.file.is_empty()
            || is_test_file(&entity.file)
            || is_test_entity(&entity.id)
            || !matches!(entity.entity_type, NodeType::Function)
        {
            continue;
        }
        let count = refs.get(&idx).copied().unwrap_or(0);
        if count < 2 {
            continue; // used once is not yet a building block
        }
        let key = entity
            .effect_signature
            .0
            .first()
            .map(|e| effect_label(*e))
            .unwrap_or("pure");
        groups
            .entry(key)
            .or_default()
            .push((count, entity.id.clone()));
    }

    let mut keys: Vec<&'static str> = groups.keys().copied().collect();
    keys.sort();
    let mut lines = Vec::new();
    for key in keys {
        let items = groups.get_mut(key).expect("key came from the map");
        // Most-referenced first; id as the tiebreak so output is stable.
        items.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let listed: Vec<String> = items
            .iter()
            .take(PER_GROUP)
            .map(|(n, id)| format!("{id} ({n} refs)"))
            .collect();
        lines.push(format!("  {}: {}", key, listed.join(", ")));
    }
    lines
}

/// Render the world model for `repo`, or `None` when there is nothing useful to
/// say (no policy and no graph) — silence beats a section header with no facts.
/// `env` is the repo's environment-variable surface (name, how many modules read
/// it) from [`crate::config::env_vars`]. Passed in rather than read here so this
/// module stays free of file I/O.
pub fn render(
    policy: &Policy,
    built: Option<&BuiltGraph>,
    env: &[(String, usize)],
    budget: usize,
) -> Option<String> {
    let mut out = vec![
        "slop: facts about this codebase, from its call/effect graph.".to_string(),
    ];

    if let Some(lines) = channel_lines(policy) {
        out.push(String::new());
        out.push("Effects are routed through these channels; code elsewhere calls them rather than acquiring the effect directly:".to_string());
        out.extend(lines);
    }

    if !env.is_empty() {
        out.push(String::new());
        // Named, with the spread, so an agent needing a setting reaches for one
        // that exists instead of inventing a parallel name for it.
        let listed: Vec<String> = env
            .iter()
            .take(ENV_LIMIT)
            .map(|(var, n)| if *n > 1 { format!("{var} ({n} modules)") } else { var.clone() })
            .collect();
        out.push(format!(
            "Configuration this codebase reads from the environment: {}",
            listed.join(", ")
        ));
    }

    let layers = layer_lines(policy);
    if !layers.is_empty() {
        out.push(String::new());
        out.push("Layer rules:".to_string());
        out.extend(layers);
    }

    let mut caps = built.map(capability_lines).unwrap_or_default();
    if !caps.is_empty() {
        out.push(String::new());
        out.push(
            "Existing reusable functions, by effect. Prefer these over new implementations:"
                .to_string(),
        );
        // Trim from the least useful end (highest effect label, i.e. last group)
        // until the whole model fits the host's context cap.
        while !caps.is_empty()
            && out.iter().map(|l| l.len() + 1).sum::<usize>()
                + caps.iter().map(|l| l.len() + 1).sum::<usize>()
                > budget
        {
            caps.pop();
        }
        out.extend(caps);
    }

    // Only the lead line means we learned nothing worth injecting.
    (out.len() > 1).then(|| out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Policy {
        toml::from_str(
            "[channels]\nnet = [\"core.http_client.HttpClient\"]\n\n[[layer]]\nname = \"pure-utils\"\nmatch = [\"utils.scoring\"]\nforbid = [\"net\", \"fs\"]\n",
        )
        .unwrap()
    }

    #[test]
    fn renders_channels_and_layers_from_policy_alone() {
        let out = render(&policy(), None, &[], DEFAULT_BUDGET).unwrap();
        assert!(out.contains("net: core.http_client.HttpClient"));
        assert!(out.contains("pure-utils (utils.scoring) does not perform: net, fs"));
    }

    #[test]
    fn no_policy_and_no_graph_says_nothing() {
        assert!(render(&Policy::default(), None, &[], DEFAULT_BUDGET).is_none());
    }

    #[test]
    fn budget_trims_capabilities_not_policy() {
        // A tiny budget must still deliver the channel facts, which are the
        // load-bearing part; capabilities are what gets dropped.
        let out = render(&policy(), None, &[], 1).unwrap();
        assert!(out.contains("core.http_client.HttpClient"));
    }
    #[test]
    fn the_env_surface_is_named_with_its_spread() {
        let env = vec![("DATABASE_URL".to_string(), 4), ("TZ".to_string(), 1)];
        let out = render(&policy(), None, &env, DEFAULT_BUDGET).unwrap();
        assert!(out.contains("DATABASE_URL (4 modules)"), "{out}");
        // A single-module read needs no count.
        assert!(out.contains("TZ") && !out.contains("TZ (1"), "{out}");
    }

}
