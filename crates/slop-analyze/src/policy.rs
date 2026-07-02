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
