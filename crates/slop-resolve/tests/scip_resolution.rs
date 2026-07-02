//! M0 exit criterion: the resolution adapter returns correct definitions
//! for the 3-file toy repo (tests/fixtures/toy_repo).
//!
//! Regenerate the fixture index with:
//!   npx --yes @sourcegraph/scip-python index tests/fixtures/toy_repo \
//!     --project-name toy-repo --output tests/fixtures/toy_repo/index.scip

use std::path::PathBuf;

use slop_resolve::{Resolver, ScipResolver};

fn resolver() -> ScipResolver {
    let index = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/toy_repo/index.scip");
    ScipResolver::load(&index).expect("fixture index should load")
}

#[test]
fn indexes_all_fixture_files() {
    let r = resolver();
    let files = r.files();
    for expected in [
        "core/http_client.py",
        "services/weather.py",
        "utils/dates.py",
    ] {
        assert!(files.contains(&expected), "missing {expected} in {files:?}");
    }
}

#[test]
fn resolves_cross_module_function_call() {
    // services/weather.py:10  `when = parse_date(day)`
    let r = resolver();
    let def = r
        .resolve("services/weather.py", 10, 11)
        .expect("parse_date should resolve");
    assert_eq!(def.display_name, "parse_date()");
    assert_eq!(def.file.as_deref(), Some("utils/dates.py"));
    assert_eq!(def.range.unwrap().start_line, 5);
}

#[test]
fn resolves_method_call_through_inferred_receiver_type() {
    // services/weather.py:11  `return client.get(...)` — requires pyright-grade
    // type inference on `client: HttpClient`. This is the case a naive
    // resolver can't do and the reason D5 chose scip-python.
    let r = resolver();
    let def = r
        .resolve("services/weather.py", 11, 18)
        .expect("client.get should resolve");
    assert_eq!(def.display_name, "HttpClient#get()");
    assert_eq!(def.file.as_deref(), Some("core/http_client.py"));
    assert_eq!(def.range.unwrap().start_line, 12);
}

#[test]
fn resolves_imported_class_to_its_definition() {
    // services/weather.py:4  `from core.http_client import HttpClient`
    let r = resolver();
    let def = r
        .resolve("services/weather.py", 4, 29)
        .expect("HttpClient import should resolve");
    assert_eq!(def.file.as_deref(), Some("core/http_client.py"));
    assert_eq!(def.range.unwrap().start_line, 5);
}

#[test]
fn resolves_same_file_call() {
    // services/weather.py:17  `return fetch_forecast(client, ...)`
    let r = resolver();
    let def = r
        .resolve("services/weather.py", 17, 11)
        .expect("fetch_forecast should resolve");
    assert_eq!(def.file.as_deref(), Some("services/weather.py"));
    assert_eq!(def.range.unwrap().start_line, 8);
}

#[test]
fn resolves_stdlib_module_reference_as_external() {
    // core/http_client.py:18  `urllib.request.urlopen(url)` — the module
    // reference must resolve externally; the Net seed table keys off it.
    let r = resolver();
    let def = r
        .resolve("core/http_client.py", 18, 22)
        .expect("urllib.request should resolve");
    assert!(def.is_external());
    assert!(
        def.symbol.contains("urllib.request"),
        "unexpected symbol {}",
        def.symbol
    );
}

#[test]
fn local_symbols_do_not_collide_across_files() {
    // `local 0` exists in more than one fixture file; each must resolve to
    // its own document-scoped definition.
    let r = resolver();
    // core/http_client.py:18 `urlopen(url)` — `url` is `local 0`, defined at 14:8.
    let def = r
        .resolve("core/http_client.py", 18, 45)
        .expect("local `url` should resolve within its file");
    assert_eq!(def.file.as_deref(), Some("core/http_client.py"));
    assert_eq!(def.range.unwrap().start_line, 14);
}
