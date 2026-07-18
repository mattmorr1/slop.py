//! End-to-end for the trivial-wrapper inliner without a SCIP binary: a mock
//! resolver over real temp files drives the true `build_graph -> parse_repo ->
//! plan_inlines -> disk` path. Proves a same-module identity forwarder is
//! inlined at both an in-module call site and a cross-file importer (rewriting
//! `from m import load` to the callee), the wrapper definition is deleted, and
//! every touched file still parses.

use std::collections::HashMap;

use slop_analyze::inline::{self, InlineOutcome};
use slop_analyze::findings::{Finding, Severity};
use slop_analyze::{build, source};
use slop_resolve::{Definition, Occurrence, Range, Resolver, SymbolKind};

const PKG: &str = "scip-python python pkg 1.0";

fn range(sl: u32, sc: u32, el: u32, ec: u32) -> Range {
    Range { start_line: sl, start_col: sc, end_line: el, end_col: ec }
}

struct MockResolver {
    by_file: HashMap<String, Vec<Occurrence>>,
    defs: HashMap<String, Definition>,
}

impl Resolver for MockResolver {
    fn resolve(&self, _f: &str, _l: u32, _c: u32) -> Option<&Definition> {
        None
    }
    fn definition_of(&self, symbol: &str) -> Option<&Definition> {
        self.defs.get(symbol)
    }
    fn occurrences_in(&self, file: &str) -> &[Occurrence] {
        self.by_file.get(file).map(Vec::as_slice).unwrap_or(&[])
    }
    fn files(&self) -> Vec<&str> {
        let mut fs: Vec<&str> = self.by_file.keys().map(String::as_str).collect();
        fs.sort();
        fs
    }
}

fn occ(symbol: &str, r: Range, is_def: bool, enclosing: Option<Range>) -> Occurrence {
    Occurrence { symbol: symbol.to_string(), range: r, is_definition: is_def, enclosing_range: enclosing }
}

fn def(symbol: &str, file: &str, kind: SymbolKind, name: &str) -> Definition {
    Definition {
        symbol: symbol.to_string(),
        file: Some(file.to_string()),
        range: None,
        enclosing_range: None,
        kind,
        display_name: name.to_string(),
        documentation: vec![],
    }
}

#[test]
fn inlines_same_module_identity_forwarder_across_files() {
    let dir = std::env::temp_dir().join(format!("slop-inline-e2e-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let m = "def read_file(path):\n    return open(path).read()\n\n\ndef load(path):\n    return read_file(path)\n\n\ndef caller():\n    return load(\"x.txt\")\n";
    let u = "from m import load\n\nprint(load(\"y\"))\n";
    std::fs::write(dir.join("m.py"), m).unwrap();
    std::fs::write(dir.join("u.py"), u).unwrap();

    let module = format!("{PKG} `m`/__init__:");
    let read_file = format!("{PKG} `m`/read_file().");
    let load = format!("{PKG} `m`/load().");
    let caller = format!("{PKG} `m`/caller().");

    let m_occs = vec![
        occ(&module, range(0, 0, 0, 1), true, None),
        occ(&read_file, range(0, 4, 0, 13), true, Some(range(0, 0, 1, 28))),
        occ(&load, range(4, 4, 4, 8), true, Some(range(4, 0, 5, 26))),
        occ(&read_file, range(5, 11, 5, 20), false, None), // read_file(path) inside load
        occ(&caller, range(8, 4, 8, 10), true, Some(range(8, 0, 9, 24))),
        occ(&load, range(9, 11, 9, 15), false, None), // load("x.txt") inside caller
    ];
    let u_occs = vec![
        occ(&load, range(0, 14, 0, 18), false, None), // from m import load
        occ(&load, range(2, 6, 2, 10), false, None),  // print(load("y"))
    ];
    let defs = [
        def(&module, "m.py", SymbolKind::Module, "m"),
        def(&read_file, "m.py", SymbolKind::Function, "read_file"),
        def(&load, "m.py", SymbolKind::Function, "load"),
        def(&caller, "m.py", SymbolKind::Function, "caller"),
    ]
    .into_iter()
    .map(|d| (d.symbol.clone(), d))
    .collect();
    let resolver = MockResolver {
        by_file: HashMap::from([("m.py".to_string(), m_occs), ("u.py".to_string(), u_occs)]),
        defs,
    };
    let built = build::build_graph(&resolver);
    let facts = source::parse_repo(&dir, &["m.py", "u.py"]);

    let finding = Finding {
        rule: "trivial-wrapper",
        severity: Severity::Warning,
        entity: "m::load".to_string(),
        file: "m.py".to_string(),
        lines: (4, 5),
        message: String::new(),
        fix_guidance: String::new(),
    };
    let result = inline::plan_inlines(&built, &resolver, &dir, &facts, &[finding]);

    let planned: Vec<_> = result
        .outcomes
        .iter()
        .filter_map(|o| match o {
            InlineOutcome::Planned(p) => Some(p),
            _ => None,
        })
        .collect();
    assert_eq!(planned.len(), 1, "the identity forwarder should be planned");
    assert_eq!(planned[0].entity, "m::load");
    assert_eq!(planned[0].callee, "read_file");
    assert_eq!(planned[0].occurrences, 3, "caller + import + module-level call");
    assert_eq!(planned[0].file_count, 2);

    let out_m = &result.files["m.py"];
    assert!(!out_m.contains("def load("), "wrapper definition deleted: {out_m}");
    assert!(out_m.contains("return read_file(\"x.txt\")"), "in-module call inlined: {out_m}");
    assert!(out_m.contains("def read_file(path):"), "callee untouched: {out_m}");
    assert!(slop_parse::Language::Python.parse(out_m).is_ok(), "m.py must parse: {out_m}");

    let out_u = &result.files["u.py"];
    assert!(out_u.contains("from m import read_file"), "import rewritten: {out_u}");
    assert!(out_u.contains("print(read_file(\"y\"))"), "cross-file call inlined: {out_u}");
    assert!(!out_u.contains("load"), "no dangling load reference: {out_u}");

    let _ = std::fs::remove_dir_all(&dir);
}
