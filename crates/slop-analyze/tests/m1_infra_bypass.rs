//! M1 exit criterion: the walking skeleton fires on a slopped repo and
//! stays silent on clean code, through every architectural layer
//! (resolve -> graph -> effect -> detector -> severity).

use std::path::PathBuf;

use slop_analyze::{build, detect, effects, policy::Policy};
use slop_graph::Effect;
use slop_resolve::ScipResolver;

fn analyze(fixture: &str, with_policy: bool) -> (build::BuiltGraph, Vec<slop_analyze::findings::Finding>) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures").join(fixture);
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

#[test]
fn clean_repo_is_silent() {
    let (_, findings) = analyze("toy_repo", true);
    assert!(findings.is_empty(), "unexpected findings: {findings:#?}");
}

#[test]
fn slopped_repo_fires_exactly_one_blocking_bypass() {
    let (_, findings) = analyze("toy_repo_slopped", true);
    assert_eq!(findings.len(), 1, "findings: {findings:#?}");
    let f = &findings[0];
    assert_eq!(f.rule, "infra-bypass");
    assert_eq!(f.severity, slop_analyze::findings::Severity::Blocking);
    assert_eq!(f.entity, "services.alerts::send_alert");
    assert_eq!(f.file, "services/alerts.py");
}

#[test]
fn empty_policy_keeps_bypass_silent() {
    // D8: no confirmed sanctioned channels => report nothing.
    let (_, findings) = analyze("toy_repo_slopped", false);
    assert!(findings.is_empty(), "unexpected findings: {findings:#?}");
}

#[test]
fn sanctioned_channel_is_never_flagged_for_its_own_effect() {
    // HttpClient.get calls urllib directly — that is its job.
    let (built, findings) = analyze("toy_repo_slopped", true);
    assert!(findings.iter().all(|f| !f.entity.starts_with("core.http_client")));
    // ...and it did acquire the effect.
    let get = built.graph.node("core.http_client::HttpClient::get").expect("get node");
    assert!(built.graph.entity(get).effect_signature.contains(Effect::Net));
}

#[test]
fn transitive_effects_propagate_but_do_not_flag() {
    // weather.fetch_forecast reaches Net only through the sanctioned
    // channel: the effect must propagate (for effect-creep, M2) without
    // producing a bypass finding.
    let (built, findings) = analyze("toy_repo_slopped", true);
    let fetch = built
        .graph
        .node("services.weather::fetch_forecast")
        .expect("fetch_forecast node");
    assert!(built.graph.entity(fetch).effect_signature.contains(Effect::Net));
    assert!(findings.iter().all(|f| !f.entity.starts_with("services.weather")));
}
