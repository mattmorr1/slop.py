//! Fixture-driven detector tests. Clean repo must be silent; the slopped
//! repo must fire each deterministic detector on exactly the planted slop.

use std::path::PathBuf;

use slop_analyze::findings::{Finding, Severity};
use slop_analyze::{build, detect, effects, policy::Policy};
use slop_graph::Effect;
use slop_resolve::ScipResolver;

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
    let findings = detect::run_all(&built, &policy);
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
            "utils.scoring::compute_risk_score"
        ]
    );
}

#[test]
fn purity_lie_flags_compute_named_io() {
    let (_, findings) = analyze("toy_repo_slopped", true);
    assert_eq!(
        entities_for(&findings, "purity-lie"),
        vec!["utils.scoring::compute_risk_score"]
    );
    let f = findings.iter().find(|f| f.rule == "purity-lie").unwrap();
    assert_eq!(f.severity, Severity::Warning);
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
