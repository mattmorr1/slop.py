//! Multi-language validation: the effect engine works over a real
//! `scip-typescript` index (fixture `ts_probe`, generated from a tiny project
//! using axios / fs / child_process / process.env). This is the end-to-end
//! check that `external_effect_id` + the JS seed table actually resolve — the
//! caveat the JS seeds shipped with until this ran.

use std::path::PathBuf;

use slop_analyze::findings::Finding;
use slop_analyze::policy::Policy;
use slop_analyze::{build, detect, effects, source};
use slop_graph::{Effect, NodeType};
use slop_resolve::{Resolver, ScipResolver};

fn effect_sig(built: &build::BuiltGraph, name: &str) -> Vec<Effect> {
    built
        .graph
        .entities()
        .find(|(_, e)| e.entity_type == NodeType::Function && e.id.ends_with(name))
        .map(|(_, e)| e.effect_signature.0.clone())
        .unwrap_or_default()
}

#[test]
fn typescript_effects_propagate_through_external_seeds() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/ts_probe");
    let resolver = ScipResolver::load(&root.join("index.scip")).expect("ts_probe index");
    let mut built = build::build_graph(&resolver);
    effects::infer_effects(&mut built);

    // axios.get -> Net
    assert!(
        effect_sig(&built, "fetchUser").contains(&Effect::Net),
        "axios should seed Net into fetchUser"
    );
    // fs.readFileSync -> FsRead/FsWrite
    let read = effect_sig(&built, "readConfig");
    assert!(
        read.contains(&Effect::FsRead) || read.contains(&Effect::FsWrite),
        "fs should seed a filesystem effect into readConfig, got {read:?}"
    );
    // child_process.exec -> Concurrency, process.env -> Env
    let run = effect_sig(&built, "runCmd");
    assert!(
        run.contains(&Effect::Concurrency),
        "child_process should seed Concurrency into runCmd, got {run:?}"
    );
    assert!(
        run.contains(&Effect::Env),
        "process.env should seed Env into runCmd, got {run:?}"
    );
}

#[test]
fn parser_based_detectors_fire_on_typescript() {
    // The tree-sitter JS/TS parser feeds the same duplication/complexity
    // detectors as Python — and the facts join back to the scip-typescript
    // graph entities by line.
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/ts_probe");
    let resolver = ScipResolver::load(&root.join("index.scip")).expect("ts_probe index");
    let mut built = build::build_graph(&resolver);
    effects::infer_effects(&mut built);
    let facts = source::parse_repo(&root, &resolver.files());

    // The TS file was parsed into per-function facts (not skipped).
    assert!(
        facts.iter().any(|f| f.file.ends_with(".ts") && !f.functions.is_empty()),
        "the .ts file should yield parsed function facts"
    );

    let findings: Vec<Finding> = detect::run_all(&built, &Policy::default(), &facts, &root);
    let has = |rule: &str, needle: &str| {
        findings
            .iter()
            .any(|f| f.rule == rule && f.entity.contains(needle))
    };

    // sumA/sumB have identical bodies -> duplicate-exact, joined to a named entity.
    assert!(
        has("duplicate-exact", "sum"),
        "duplicate-exact should fire on sumA/sumB: {:#?}",
        findings.iter().map(|f| (f.rule, &f.entity)).collect::<Vec<_>>()
    );
    // `tangled` nests for->for->if->while = depth 4 -> complexity-spike.
    assert!(
        has("complexity-spike", "tangled"),
        "complexity-spike should fire on `tangled`"
    );
}
