//! Deterministic, provenance-bearing context projected from one repository snapshot.

use serde::Serialize;

use crate::compress::{self, CompressConfig, CompressStats};
use crate::envelope::{self, EnvelopeConfig, EnvelopeItem, Selection};
use crate::snapshot::{DocumentState, RepositorySnapshot, SnapshotFreshness, SnapshotId};

pub const CONTEXT_SCHEMA_VERSION: u32 = 4;

#[derive(Debug, Clone)]
pub struct ContextRequest<'a> {
    pub target_entity: &'a str,
    pub token_budget: usize,
    pub edit_zone_hops: usize,
    pub selection: Selection,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextFallback {
    pub reason: &'static str,
    pub file: Option<String>,
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextArtifact {
    pub schema_version: u32,
    pub snapshot: SnapshotId,
    pub freshness: SnapshotFreshness,
    pub target: String,
    pub token_budget: usize,
    pub edit_zone_hops: usize,
    pub estimated_tokens: usize,
    pub items: Vec<EnvelopeItem>,
    /// Selected entities left out because their document is not resolved.
    pub omitted_unresolved: usize,
    pub fallback: Option<ContextFallback>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "representation", rename_all = "snake_case")]
pub enum ReadContext {
    Compressed {
        snapshot: SnapshotId,
        source: String,
        stats: CompressStats,
    },
    Verbatim {
        snapshot: SnapshotId,
        reason: &'static str,
    },
}

/// One entity's verbatim source: the inverse of a skeleton in a context artifact.
#[derive(Debug, Clone, Serialize)]
pub struct Expansion {
    pub schema_version: u32,
    pub snapshot: SnapshotId,
    pub entity: String,
    pub file: String,
    /// 0-based inclusive line range in `file`.
    pub lines: (usize, usize),
    /// Anything but `resolved` means the range came from an index built from other content.
    pub state: DocumentState,
    pub text: String,
}

impl RepositorySnapshot {
    /// Expand one entity from the same snapshot its skeleton came from, so an agent
    /// can reverse compression per item without mixing source generations.
    pub fn expand(&self, entity_id: &str) -> Option<Expansion> {
        let entity = self.built.graph.entity(self.built.graph.node(entity_id)?);
        let source = self.sources.get(&entity.file)?;
        let (start, end) = entity.source_range;
        let text: Vec<&str> = source.lines().skip(start).take(end.checked_sub(start)? + 1).collect();
        (!text.is_empty()).then(|| Expansion {
            schema_version: CONTEXT_SCHEMA_VERSION,
            snapshot: self.id().clone(),
            entity: entity.id.clone(),
            file: entity.file.clone(),
            lines: (start, end),
            state: self.document_state(&entity.file),
            text: text.join("\n"),
        })
    }

    pub fn context(&self, request: ContextRequest<'_>) -> ContextArtifact {
        let file = self
            .built
            .graph
            .node(request.target_entity)
            .map(|idx| self.built.graph.entity(idx).file.clone());
        let fallback = match file.as_deref().map(|path| self.document_state(path)) {
            None | Some(DocumentState::Resolved) => None,
            Some(_) => {
                let text = file
                    .as_ref()
                    .and_then(|path| self.sources.get(path))
                    .map(|source| source.to_string());
                Some(ContextFallback {
                    reason: "stale_index_verbatim",
                    file,
                    text,
                })
            }
        };
        let (items, omitted_unresolved) = if fallback.is_none() {
            envelope::build_captured_envelope(
                &self.built,
                &self.facts,
                &self.sources,
                request.target_entity,
                &EnvelopeConfig {
                    token_budget: request.token_budget,
                    edit_zone_hops: request.edit_zone_hops,
                    selection: request.selection,
                },
                &envelope::Relevance { model: &self.relevance, lexical: self.lexical() },
                |file| self.document_state(file) == DocumentState::Resolved,
            )
        } else {
            (Vec::new(), 0)
        };
        let fallback = if fallback.is_none() && items.is_empty() {
            Some(ContextFallback {
                reason: "unknown_target",
                file: None,
                text: None,
            })
        } else {
            fallback
        };
        let estimated_tokens = items
            .iter()
            .map(|item| (item.text.len() + item.equivalents.iter().map(String::len).sum::<usize>()) / 4 + 1)
            .sum::<usize>()
            + fallback
                .as_ref()
                .and_then(|value| value.text.as_ref())
                .map_or(0, |text| text.len() / 4 + 1);
        ContextArtifact {
            schema_version: CONTEXT_SCHEMA_VERSION,
            snapshot: self.id().clone(),
            freshness: self.freshness().clone(),
            target: request.target_entity.to_string(),
            token_budget: request.token_budget,
            edit_zone_hops: request.edit_zone_hops,
            estimated_tokens,
            items,
            omitted_unresolved,
            fallback,
        }
    }

    pub fn compress_read(
        &self,
        rel_file: &str,
        source: &str,
        edit_files: &[String],
        config: &CompressConfig,
    ) -> ReadContext {
        let verbatim = |reason| ReadContext::Verbatim {
            snapshot: self.id().clone(),
            reason,
        };
        if source.lines().count() < config.min_lines {
            return verbatim("small_file");
        }
        let resolved = |file: &str| self.document_state(file) == DocumentState::Resolved;
        if !resolved(rel_file) || !edit_files.iter().all(|file| resolved(file)) {
            return verbatim("stale_index");
        }
        if self
            .sources
            .get(rel_file)
            .is_none_or(|captured| captured.as_ref() != source)
        {
            return verbatim("source_changed");
        }
        let loci: Vec<String> = self
            .built
            .graph
            .entities()
            .filter(|(_, entity)| edit_files.contains(&entity.file))
            .map(|(_, entity)| entity.id.clone())
            .collect();
        if loci.is_empty() {
            return verbatim("empty_edit_zone");
        }
        let (source, stats) = compress::compress_file(
            &self.built,
            &self.facts,
            source,
            rel_file,
            &loci,
            config,
            None,
        );
        if stats.skeletonized == 0 {
            return verbatim("no_compression_gain");
        }
        ReadContext::Compressed {
            snapshot: self.id().clone(),
            source,
            stats,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::snapshot::{CaptureRequest, Freshness, RepositorySnapshot};
    use crate::test_support::TempFixture;

    use super::*;

    fn fixture(name: &str) -> TempFixture {
        TempFixture::new(name)
    }

    #[test]
    fn identical_request_is_byte_deterministic() {
        let repo = fixture("toy_repo");
        let snapshot = RepositorySnapshot::capture(CaptureRequest {
            repo: &repo,
            index: None,
            policy: None,
            freshness: Freshness::Warn,
        })
        .expect("snapshot");
        let render = || {
            serde_json::to_vec(&snapshot.context(ContextRequest {
                target_entity: "core.http_client::HttpClient::get",
                token_budget: 8000,
                edit_zone_hops: 1,
                selection: Selection::Coverage,
            }))
            .expect("serialize")
        };
        let expected = render();
        for _ in 0..10 {
            assert_eq!(render(), expected);
        }
    }

    #[test]
    fn unknown_target_is_an_explicit_fallback() {
        let repo = fixture("toy_repo");
        let snapshot = RepositorySnapshot::capture(CaptureRequest {
            repo: &repo,
            index: None,
            policy: None,
            freshness: Freshness::Warn,
        })
        .expect("snapshot");
        let artifact = snapshot.context(ContextRequest {
            target_entity: "missing::entity",
            token_budget: 8000,
            edit_zone_hops: 1,
            selection: Selection::Coverage,
        });
        assert_eq!(
            artifact.fallback.expect("fallback").reason,
            "unknown_target"
        );
    }

    #[test]
    #[ignore = "manual workspace benchmark"]
    fn benchmark_warm_workspace_context() {
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let snapshot = RepositorySnapshot::capture(CaptureRequest {
            repo: &repo,
            index: None,
            policy: None,
            freshness: Freshness::Warn,
        })
        .expect("snapshot");
        let mut samples = Vec::with_capacity(100);
        for _ in 0..100 {
            let start = std::time::Instant::now();
            let artifact = snapshot.context(ContextRequest {
                target_entity: "slop::main",
                token_budget: 8000,
                edit_zone_hops: 1,
                selection: Selection::Coverage,
            });
            assert!(artifact.fallback.is_none());
            samples.push(start.elapsed());
        }
        samples.sort();
        eprintln!(
            "warm context p50={:?} p95={:?} p99={:?}",
            samples[49], samples[94], samples[98]
        );
    }
}
