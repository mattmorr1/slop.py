//! Fixture-driven detector tests. Clean repo must be silent; the slopped
//! repo must fire each deterministic detector on exactly the planted slop.

use std::path::PathBuf;

use slop_analyze::findings::{Finding, Severity};
use slop_analyze::{build, detect, effects, policy::Policy};
use slop_graph::Effect;
use slop_resolve::{Resolver, ScipResolver};

fn analyze(fixture: &str, with_policy: bool) -> (build::BuiltGraph, Vec<Finding>) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(fixture);
    let resolver = ScipResolver::load(&root.join("index.scip")).expect("fixture index");
    let policy = if with_policy {
        Policy::load(&root).expect("fixture policy")
    } else {
        Policy::default()
    };
    let mut built = build::build_graph(&resolver);
    effects::infer_effects(&mut built);
    let facts = slop_analyze::source::parse_repo(&root, &resolver.files());
    let findings = detect::run_all(&built, &policy, &facts, &root);
    (built, findings)
}

fn entities_for<'a>(findings: &'a [Finding], rule: &str) -> Vec<&'a str> {
    let mut v: Vec<&str> = findings
        .iter()
        .filter(|f| f.rule == rule)
        .map(|f| f.entity.as_str())
        .collect();
    v.sort();
    v.dedup();
    v
}

#[test]
fn clean_repo_is_silent() {
    let (_, findings) = analyze("toy_repo", true);
    assert!(findings.is_empty(), "unexpected findings: {findings:#?}");
}

#[test]
fn infra_bypass_fires_on_planted_bypasses_only() {
    let (_, findings) = analyze("toy_repo_slopped", true);
    assert_eq!(
        entities_for(&findings, "infra-bypass"),
        vec![
            "services.alerts::send_alert",
            "utils.scoring::compute_risk_score"
        ]
    );
    assert!(findings
        .iter()
        .filter(|f| f.rule == "infra-bypass")
        .all(|f| f.severity == Severity::Blocking));
}

#[test]
fn circular_import_finds_the_planted_cycle() {
    let (_, findings) = analyze("toy_repo_slopped", true);
    assert_eq!(
        entities_for(&findings, "circular-import"),
        vec!["services.notify", "services.reports"]
    );
    let f = findings
        .iter()
        .find(|f| f.rule == "circular-import")
        .unwrap();
    assert_eq!(f.severity, Severity::Blocking);
    assert!(f.message.contains("services.notify -> services.reports"));
}

#[test]
fn dead_island_flags_unreferenced_functions_only() {
    let (_, findings) = analyze("toy_repo_slopped", true);
    assert_eq!(
        entities_for(&findings, "dead-island"),
        vec![
            "services.alerts::send_alert",
            "services.notify::notify_with_report",
            "services.routing::apply_discount",
            "services.routing::route_event",
            "utils.cleaning::clean_rows",
            "utils.cleaning::normalize_rows",
            "utils.cleaning::scale_rows",
            "utils.scoring::compute_risk_score",
            "utils.scoring::parse_config",
            "utils.when::fetchConfigV2",
            "utils.when::to_datetime"
        ]
    );
}

#[test]
fn naming_flags_camel_deviant_and_slop_marker() {
    let (_, findings) = analyze("toy_repo_slopped", true);
    assert_eq!(
        entities_for(&findings, "naming-convention"),
        vec!["utils.when::fetchConfigV2"]
    );
    assert_eq!(
        entities_for(&findings, "slop-name"),
        vec!["utils.when::fetchConfigV2"]
    );
    assert!(findings
        .iter()
        .filter(|f| f.rule == "naming-convention" || f.rule == "slop-name")
        .all(|f| f.severity == Severity::Advisory));
}

#[test]
fn tier3_buckets_produce_the_planted_semantic_pair() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/toy_repo_slopped");
    let resolver = ScipResolver::load(&root.join("index.scip")).unwrap();
    let mut built = build::build_graph(&resolver);
    effects::infer_effects(&mut built);
    let facts = slop_analyze::source::parse_repo(&root, &resolver.files());
    let candidates = slop_analyze::tier3::candidates(&built, &facts, &root);
    let planted = candidates.iter().find(|p| {
        let pair = [p.a.entity.as_str(), p.b.entity.as_str()];
        pair.contains(&"utils.dates::parse_date") && pair.contains(&"utils.when::to_datetime")
    });
    assert!(planted.is_some(), "candidates: {:#?}", candidates.iter().map(|p| (&p.a.entity, &p.b.entity)).collect::<Vec<_>>());
    let planted = planted.unwrap();
    assert!(!planted.a.snippet.is_empty());
}

#[test]
fn tier1_duplicate_pairs_exact_bodies() {
    let (_, findings) = analyze("toy_repo_slopped", true);
    // One finding per exact-duplicate set, not one per member — the
    // representative is whichever comes first in the file, with the peer
    // named in the message/fix guidance instead of getting its own finding.
    assert_eq!(
        entities_for(&findings, "duplicate-exact"),
        vec!["utils.cleaning::clean_rows"]
    );
    let f = findings.iter().find(|f| f.rule == "duplicate-exact").unwrap();
    assert!(f.message.contains("utils.cleaning::normalize_rows"), "{}", f.message);
}

#[test]
fn tier2_flags_structural_twin_only() {
    let (_, findings) = analyze("toy_repo_slopped", true);
    // scale_rows shares shape with clean/normalize but not bytes. One
    // finding for the group, naming scale_rows as the structural peer
    // (normalize_rows is already covered by the Tier-1 finding above).
    assert_eq!(
        entities_for(&findings, "duplicate-structural"),
        vec!["utils.cleaning::clean_rows"]
    );
    let f = findings.iter().find(|f| f.rule == "duplicate-structural").unwrap();
    assert!(f.message.contains("utils.cleaning::scale_rows"), "{}", f.message);
}

#[test]
fn complexity_spike_fires_on_deeply_nested_router() {
    let (_, findings) = analyze("toy_repo_slopped", true);
    assert_eq!(
        entities_for(&findings, "complexity-spike"),
        vec!["services.routing::route_event"]
    );
    // The recalibrated detector reports the real tangle shape and anchors
    // guidance on the deepest block's line, not a generic "split branches".
    let f = findings.iter().find(|f| f.rule == "complexity-spike").unwrap();
    assert!(f.message.contains("nested 5 deep"), "{}", f.message);
    assert!(f.fix_guidance.contains("deepest block"), "{}", f.fix_guidance);
}

#[test]
fn over_commenting_fires_on_narrated_function() {
    let (_, findings) = analyze("toy_repo_slopped", true);
    assert_eq!(
        entities_for(&findings, "over-commenting"),
        vec!["services.routing::apply_discount"]
    );
    let f = findings.iter().find(|f| f.rule == "over-commenting").unwrap();
    assert_eq!(f.severity, Severity::Advisory);
}

#[test]
fn effect_layer_violation_flags_io_in_the_pure_utils_layer() {
    let (_, findings) = analyze("toy_repo_slopped", true);
    // Both scoring utils directly do I/O the `pure-utils` layer forbids:
    // compute_risk_score (net) and parse_config (fs).
    assert_eq!(
        entities_for(&findings, "effect-layer-violation"),
        vec![
            "utils.scoring::compute_risk_score",
            "utils.scoring::parse_config"
        ]
    );
    let f = findings
        .iter()
        .find(|f| f.rule == "effect-layer-violation")
        .unwrap();
    assert_eq!(f.severity, Severity::Warning);
    assert!(f.message.contains("pure-utils"), "{}", f.message);
}

#[test]
fn clean_repo_has_no_layer_violations() {
    // toy_repo declares no layers -> the detector is silent (D8).
    let (_, findings) = analyze("toy_repo", true);
    assert!(findings.iter().all(|f| f.rule != "effect-layer-violation"));
}

#[test]
fn purity_lie_flags_compute_named_io() {
    // Suppression is a delivery-layer filter; the raw detector still fires
    // on both planted lies (compute_risk_score is Net, parse_config is FS
    // through the builtin open seed).
    let (_, findings) = analyze("toy_repo_slopped", true);
    assert_eq!(
        entities_for(&findings, "purity-lie"),
        vec![
            "utils.scoring::compute_risk_score",
            "utils.scoring::parse_config"
        ]
    );
    let f = findings.iter().find(|f| f.rule == "purity-lie").unwrap();
    assert_eq!(f.severity, Severity::Warning);
}

#[test]
fn suppression_scan_silences_reasoned_allow_only() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/toy_repo_slopped");
    let (_, findings) = analyze("toy_repo_slopped", true);
    let resolver = ScipResolver::load(&root.join("index.scip")).unwrap();
    let facts = slop_analyze::source::parse_repo(&root, &resolver.files());
    let suppressions = slop_analyze::suppress::scan(&root, &facts);
    assert_eq!(suppressions.len(), 1);
    assert_eq!(suppressions[0].rule, "purity-lie");
    let kept = slop_analyze::suppress::filter(findings, &suppressions);
    assert_eq!(
        entities_for(&kept, "purity-lie"),
        vec!["utils.scoring::parse_config"]
    );
}

#[test]
fn baseline_grandfathers_everything() {
    let (_, findings) = analyze("toy_repo_slopped", true);
    let baseline = slop_analyze::baseline::Baseline::from_findings(&findings);
    let kept = baseline.filter(findings);
    assert!(kept.is_empty());
}

#[test]
fn channel_inference_proposes_the_clean_repo_convention() {
    let (built, _) = analyze("toy_repo", true);
    let proposals = slop_analyze::infer::infer_channels(&built);
    assert_eq!(
        proposals.get("net").map(Vec::as_slice),
        Some(&["core.http_client.HttpClient".to_string()][..])
    );
}

#[test]
fn channel_inference_stays_silent_without_dominance() {
    // Three unrelated Net acquirers in the slopped repo: no 80% winner.
    let (built, _) = analyze("toy_repo_slopped", true);
    let proposals = slop_analyze::infer::infer_channels(&built);
    assert!(!proposals.contains_key("net"), "{proposals:?}");
}

#[test]
fn health_scores_clean_above_slopped() {
    let (clean_built, _) = analyze("toy_repo", true);
    let (slop_built, slop_findings) = analyze("toy_repo_slopped", true);
    let clean_score = slop_analyze::health::score(&[], &clean_built);
    let slop_score = slop_analyze::health::score(&slop_findings, &slop_built);
    assert_eq!(clean_score, 100);
    assert!(slop_score < 60, "slopped repo scored {slop_score}");
}

#[test]
fn empty_policy_silences_policy_gated_detectors_only() {
    // D8: no channels => no infra-bypass. Non-policy detectors still run
    // (but dead-island loses its entry_points exemptions).
    let (_, findings) = analyze("toy_repo_slopped", false);
    assert!(entities_for(&findings, "infra-bypass").is_empty());
    assert!(!entities_for(&findings, "circular-import").is_empty());
}

#[test]
fn sanctioned_channel_is_never_flagged_for_its_own_effect() {
    let (built, findings) = analyze("toy_repo_slopped", true);
    assert!(findings
        .iter()
        .filter(|f| f.rule == "infra-bypass")
        .all(|f| !f.entity.starts_with("core.http_client")));
    let get = built
        .graph
        .node("core.http_client::HttpClient::get")
        .expect("get node");
    assert!(built.graph.entity(get).effect_signature.contains(Effect::Net));
}

#[test]
fn transitive_effects_propagate_but_do_not_flag() {
    let (built, findings) = analyze("toy_repo_slopped", true);
    let fetch = built
        .graph
        .node("services.weather::fetch_forecast")
        .expect("fetch_forecast node");
    assert!(built.graph.entity(fetch).effect_signature.contains(Effect::Net));
    assert!(findings.iter().all(|f| !f.entity.starts_with("services.weather")));
}

#[test]
fn nondeterminism_seeds_through_datetime_now() {
    let (built, _) = analyze("toy_repo_slopped", true);
    let latest = built
        .graph
        .node("services.weather::latest_forecast")
        .expect("latest_forecast node");
    assert!(built
        .graph
        .entity(latest)
        .effect_signature
        .contains(Effect::Nondeterminism));
}
