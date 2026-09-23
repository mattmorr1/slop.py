//! Write-time pre-check (W1): judge *proposed* file content against the
//! sanctioned-channel policy with no SCIP index and no graph build.
//!
//! [`crate::check`] reads on-disk state, which a `PreToolUse` hook does not have
//! — the edit hasn't landed yet. So this path is deliberately content-local:
//! take the qualified names the proposed text references, ask the effect seed
//! table what they acquire, and ask the policy whether that effect already has a
//! sanctioned channel. Milliseconds, so it is cheap enough to run before every
//! write.
//!
//! Narrower than [`crate::detect`] by construction: without the graph there is
//! no transitive effect propagation, so this sees *direct* acquisition only.
//! That is exactly the infra-bypass signal, and the deeper checks still run in
//! `validate_change` / `slop gate`.

use std::collections::BTreeMap;

use serde::Serialize;
use slop_graph::Effect;
use slop_parse::{names::qualified_names, Language};

use crate::effects::seed_effects_for;
use crate::policy::Policy;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrecheckFinding {
    pub effect: Effect,
    /// The unsanctioned name the proposed content acquires it through.
    pub acquired: String,
    /// What the repo routes this effect through instead.
    pub channels: Vec<String>,
}

impl PrecheckFinding {
    /// Agent-facing steering. Stated as fact plus the concrete alternative —
    /// the hook has to be actionable in one line, since it competes with
    /// everything else in the context window.
    pub fn steering(&self) -> String {
        format!(
            "slop: `{}` acquires {:?} directly, but this codebase routes {:?} through {}. Reuse it instead of adding a parallel path.",
            self.acquired,
            self.effect,
            self.effect,
            self.channels.join(" or ")
        )
    }
}

/// Effects that proposed `source` acquires directly and that the repo already
/// has a sanctioned channel for. Empty when the repo has no policy (D8: no
/// cold-start noise) or when the edited file *is* the channel.
///
/// `rel_file` is the repo-relative path of the file being written; it is what
/// exempts a channel's own implementation, which must acquire the effect
/// directly — that is its job.
pub fn precheck(
    lang: Language,
    rel_file: &str,
    source: &str,
    policy: &Policy,
) -> Vec<PrecheckFinding> {
    if policy.channels.is_empty() {
        return Vec::new();
    }
    let module = module_path_of(rel_file);
    // One finding per bypassed effect, not per name: `import requests` and
    // `requests.post(...)` are the same violation, and the steering line the
    // agent needs is "use the channel", said once. Keep the most specific name
    // as the evidence.
    let mut worst: BTreeMap<Effect, String> = BTreeMap::new();
    for name in qualified_names(lang, source) {
        for effect in seed_effects_for(&name) {
            let channels = policy.channels_for(effect);
            if channels.is_empty() || file_defines_channel(&module, channels) {
                continue;
            }
            worst
                .entry(effect)
                .and_modify(|cur| {
                    if name.len() > cur.len() {
                        *cur = name.clone();
                    }
                })
                .or_insert_with(|| name.clone());
        }
    }
    worst
        .into_iter()
        .map(|(effect, acquired)| PrecheckFinding {
            effect,
            acquired,
            channels: policy.channels_for(effect).to_vec(),
        })
        .collect()
}

/// Does one of `channels` live in `module`? A channel's own implementation
/// acquires the effect directly — that is its whole job — so the file defining
/// it is never a bypass. Note the containment runs opposite to
/// [`Policy::is_sanctioned`]: a channel path (`core.http_client.HttpClient`) is
/// *deeper* than the module path of the file that defines it.
fn file_defines_channel(module: &str, channels: &[String]) -> bool {
    if module.is_empty() {
        return false;
    }
    channels.iter().any(|chan| {
        chan == module || (chan.starts_with(module) && chan[module.len()..].starts_with('.'))
    })
}

/// A repo-relative path as the dotted module path the policy matches against:
/// `core/http/client.py` -> `core.http.client`. Rust's crate-prefixed entity IDs
/// don't derive from the path this cleanly, so a `crates/<name>/src/` prefix is
/// folded to the crate name.
///
/// ponytail: path-shaped heuristic, not the real entity ID — the graph-backed
/// detectors resolve properly. Swap to a resolver lookup if this misfires.
fn module_path_of(rel_file: &str) -> String {
    let p = rel_file.replace('\\', "/");
    let p = p.rsplit_once('.').map_or(p.as_str(), |(stem, _)| stem);
    let mut segments: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    if segments.first() == Some(&"crates") && segments.len() > 2 {
        // crates/slop-analyze/src/check -> slop-analyze.check
        segments.remove(0);
        segments.retain(|s| *s != "src");
    }
    if segments.last() == Some(&"__init__") || segments.last() == Some(&"mod") {
        segments.pop();
    }
    segments.join(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Policy {
        toml::from_str(
            "[channels]\nnet = [\"core.http_client.HttpClient\"]\ndb = [\"core.db.Session\"]",
        )
        .unwrap()
    }

    #[test]
    fn flags_direct_net_acquisition_when_a_channel_exists() {
        let src = "import requests\n\ndef send(u):\n    return requests.post(u)\n";
        let found = precheck(Language::Python, "services/alerts.py", src, &policy());
        // One finding for the effect, evidenced by the most specific name.
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].effect, Effect::Net);
        assert_eq!(found[0].acquired, "requests.post");
        assert!(found[0].steering().contains("core.http_client.HttpClient"));
    }

    #[test]
    fn the_channel_itself_is_never_a_bypass() {
        // core/http_client.py defines the sanctioned Net channel; acquiring Net
        // directly is precisely its job.
        let src = "import requests\n\nclass HttpClient:\n    def get(self, u):\n        return requests.get(u)\n";
        assert!(precheck(Language::Python, "core/http_client.py", src, &policy()).is_empty());
    }

    #[test]
    fn no_policy_stays_silent() {
        let src = "import requests\nrequests.get('u')\n";
        assert!(precheck(Language::Python, "a.py", src, &Policy::default()).is_empty());
    }

    #[test]
    fn effects_without_a_channel_are_not_flagged() {
        // The policy covers net and db only, so a filesystem write is silent
        // even though the seed table knows about it.
        let src = "import shutil\nshutil.rmtree('/tmp/x')\n";
        assert!(precheck(Language::Python, "a.py", src, &policy()).is_empty());
    }

    #[test]
    fn works_across_languages() {
        let rust = precheck(
            Language::Rust,
            "crates/slop-tui/src/app.rs",
            "fn f() { let _ = reqwest::blocking::get(\"u\"); }",
            &policy(),
        );
        assert_eq!(rust.len(), 1, "{rust:?}");
        assert_eq!(rust[0].acquired, "reqwest.blocking.get");

        let js = precheck(
            Language::JavaScript,
            "src/alerts.js",
            "import axios from 'axios';\nexport const f = (u) => axios.get(u);\n",
            &policy(),
        );
        assert!(!js.is_empty());
        assert_eq!(js[0].effect, Effect::Net);
    }

    #[test]
    fn a_name_only_in_a_comment_is_not_an_acquisition() {
        let src = "# don't use requests.get here, use core.http.Client\nX = 1\n";
        assert!(precheck(Language::Python, "a.py", src, &policy()).is_empty());
    }

    #[test]
    fn module_paths_fold_package_and_crate_layout() {
        assert_eq!(module_path_of("core/http/client.py"), "core.http.client");
        assert_eq!(module_path_of("core/http/__init__.py"), "core.http");
        assert_eq!(module_path_of("crates/slop-analyze/src/check.rs"), "slop-analyze.check");
        assert_eq!(module_path_of("crates/slop-tui/src/lib.rs"), "slop-tui.lib");
    }
}
