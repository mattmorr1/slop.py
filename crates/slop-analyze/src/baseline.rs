//! Baseline file: grandfather existing findings so adoption on a real repo
//! doesn't open with a 4000-finding storm (D7/D12). Fingerprint is
//! (rule, entity) — stable across line shifts and message rewording.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use slop_graph::NodeType;

use crate::build::BuiltGraph;
use crate::findings::Finding;

pub const BASELINE_FILE: &str = ".slop-baseline.json";

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Baseline {
    pub version: u32,
    pub findings: Vec<BaselineEntry>,
    /// Per-function effect signature at baseline time (Debug effect names,
    /// sorted). The empty list marks a function that was *pure*; `effect-creep`
    /// fires when such a function later acquires I/O. `#[serde(default)]` keeps
    /// pre-effects baselines loadable.
    #[serde(default)]
    pub effects: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BaselineEntry {
    pub rule: String,
    pub entity: String,
}

impl Baseline {
    pub fn from_findings(findings: &[Finding]) -> Self {
        let mut entries: Vec<BaselineEntry> = findings
            .iter()
            .map(|f| BaselineEntry {
                rule: f.rule.to_string(),
                entity: f.entity.clone(),
            })
            .collect();
        entries.sort_by(|a, b| (&a.rule, &a.entity).cmp(&(&b.rule, &b.entity)));
        entries.dedup();
        Self {
            version: 1,
            findings: entries,
            effects: BTreeMap::new(),
        }
    }

    /// Record every function's effect signature so `effect-creep` can detect a
    /// later pure→effectful regression. Chainable off `from_findings`.
    pub fn with_effects(mut self, built: &BuiltGraph) -> Self {
        for (_, entity) in built.graph.entities() {
            if entity.entity_type == NodeType::Function {
                self.effects.insert(
                    entity.id.clone(),
                    entity.effect_signature.0.iter().map(|e| format!("{e:?}")).collect(),
                );
            }
        }
        self
    }

    pub fn load(repo_root: &Path) -> Result<Self> {
        let path = repo_root.join(BASELINE_FILE);
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self, repo_root: &Path) -> Result<()> {
        let path = repo_root.join(BASELINE_FILE);
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))
    }

    /// Drop findings already grandfathered.
    pub fn filter(&self, findings: Vec<Finding>) -> Vec<Finding> {
        let set: HashSet<(&str, &str)> = self
            .findings
            .iter()
            .map(|e| (e.rule.as_str(), e.entity.as_str()))
            .collect();
        findings
            .into_iter()
            .filter(|f| !set.contains(&(f.rule, f.entity.as_str())))
            .collect()
    }
}
