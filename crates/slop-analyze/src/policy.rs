//! Sanctioned-channel policy (D8), read from `slop.toml`. An empty policy
//! keeps infra-bypass silent — no cold-start false-positive storm.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use slop_graph::Effect;

#[derive(Debug, Default, Deserialize)]
pub struct Policy {
    /// effect name (lowercase) -> sanctioned channel entity paths, dotted:
    /// `net = ["core.http_client.HttpClient"]`
    #[serde(default)]
    pub channels: HashMap<String, Vec<String>>,
    /// Dotted entity paths that are API entry points — exempt from
    /// dead-island. Dunders, `main`, and `test_*` are exempt by default.
    #[serde(default)]
    pub entry_points: Vec<String>,
    /// Architectural layers: modules matching `match` may not *directly*
    /// perform the effects in `forbid` (generalizes "no DB in the view
    /// layer"). Empty = silent (D8), like `channels`.
    #[serde(default, rename = "layer")]
    pub layers: Vec<LayerRule>,
}

/// One architectural-layer rule (a `[[layer]]` table in `slop.toml`).
#[derive(Debug, Default, Deserialize)]
pub struct LayerRule {
    /// Human name for the layer, used in the finding message.
    pub name: String,
    /// Dotted module-path prefixes that belong to this layer.
    #[serde(default, rename = "match")]
    pub matches: Vec<String>,
    /// Effect names this layer may not directly acquire.
    #[serde(default)]
    pub forbid: Vec<String>,
}

impl LayerRule {
    /// Does `dotted` (a `.`-joined entity id) sit in this layer? Prefix match
    /// on path boundaries, same discipline as channel/entry-point matching.
    pub fn contains(&self, dotted: &str) -> bool {
        self.matches.iter().any(|m| {
            dotted == m || (dotted.starts_with(m) && dotted[m.len()..].starts_with('.'))
        })
    }

    /// The effects this layer forbids, resolved from their names.
    pub fn forbidden_effects(&self) -> Vec<Effect> {
        let mut out: Vec<Effect> = self.forbid.iter().flat_map(|n| effects_from_name(n)).collect();
        out.sort();
        out.dedup();
        out
    }
}

/// Map a policy effect name to lattice effects. `fs` covers both directions.
pub fn effects_from_name(name: &str) -> Vec<Effect> {
    match name.trim().to_lowercase().as_str() {
        "net" => vec![Effect::Net],
        "db" => vec![Effect::Db],
        "fs" => vec![Effect::FsRead, Effect::FsWrite],
        "fs_read" => vec![Effect::FsRead],
        "fs_write" => vec![Effect::FsWrite],
        "env" => vec![Effect::Env],
        "nondeterminism" | "time" | "random" => vec![Effect::Nondeterminism],
        "concurrency" => vec![Effect::Concurrency],
        "state" | "state_mutate" => vec![Effect::StateMutate],
        _ => vec![],
    }
}

impl Policy {
    pub fn load(repo_root: &Path) -> Result<Self> {
        Self::load_file(&repo_root.join("slop.toml"))
    }

    pub fn load_file(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    fn effect_key(effect: Effect) -> Option<&'static str> {
        match effect {
            Effect::Net => Some("net"),
            Effect::Db => Some("db"),
            Effect::FsRead | Effect::FsWrite => Some("fs"),
            Effect::Env => Some("env"),
            _ => None,
        }
    }

    /// Channels sanctioned for an effect; empty slice = no policy = silent.
    pub fn channels_for(&self, effect: Effect) -> &[String] {
        Self::effect_key(effect)
            .and_then(|k| self.channels.get(k))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Is this entity an API entry point (exempt from dead-island)?
    pub fn is_entry_point(&self, entity_id: &str) -> bool {
        let name = entity_id.rsplit("::").next().unwrap_or(entity_id);
        if name == "main"
            || name.starts_with("test_")
            || (name.starts_with("__") && name.ends_with("__"))
        {
            return true;
        }
        let dotted = entity_id.replace("::", ".");
        self.entry_points.iter().any(|ep| {
            dotted == *ep || (dotted.starts_with(ep) && dotted[ep.len()..].starts_with('.'))
        })
    }

    /// Is `entity_id` (`::`-separated) inside a sanctioned channel for
    /// `effect`? The channel spec is a dotted path prefix.
    pub fn is_sanctioned(&self, effect: Effect, entity_id: &str) -> bool {
        let dotted = entity_id.replace("::", ".");
        self.channels_for(effect).iter().any(|chan| {
            dotted == *chan
                || (dotted.starts_with(chan) && dotted[chan.len()..].starts_with('.'))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Policy {
        toml::from_str("[channels]\nnet = [\"core.http_client.HttpClient\"]").unwrap()
    }

    #[test]
    fn sanctions_channel_members() {
        let p = policy();
        assert!(p.is_sanctioned(Effect::Net, "core.http_client::HttpClient"));
        assert!(p.is_sanctioned(Effect::Net, "core.http_client::HttpClient::get"));
        assert!(!p.is_sanctioned(Effect::Net, "services.alerts::send_alert"));
        // Prefix must respect path boundaries.
        assert!(!p.is_sanctioned(Effect::Net, "core.http_client::HttpClientFactory"));
    }

    #[test]
    fn empty_policy_sanctions_nothing_and_has_no_channels() {
        let p = Policy::default();
        assert!(p.channels_for(Effect::Net).is_empty());
    }
}
