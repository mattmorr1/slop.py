//! End-to-end for the rename fixer without a SCIP binary: a mock resolver over
//! real temp files drives the true `build_graph -> plan_renames -> disk` path.
//! Proves a free-function rename rewrites the definition *and* its reference,
//! and that a method is skipped (framework/dynamic-dispatch safety).

use slop_analyze::build;
use slop_analyze::findings::{Finding, Severity};
use slop_analyze::rename::{self, RenameOutcome};
use slop_resolve::{Definition, Occurrence, Range, Resolver, SymbolKind};

const PKG: &str = "scip-python python pkg 1.0";

fn range(sl: u32, sc: u32, el: u32, ec: u32) -> Range {
    Range { start_line: sl, start_col: sc, end_line: el, end_col: ec }
}

struct MockResolver {
    occs: Vec<Occurrence>,
    defs: std::collections::HashMap<String, Definition>,
}

impl Resolver for MockResolver {
    fn resolve(&self, _f: &str, _l: u32, _c: u32) -> Option<&Definition> {
        None
    }
    fn definition_of(&self, symbol: &str) -> Option<&Definition> {
        self.defs.get(symbol)
    }
    fn occurrences_in(&self, _file: &str) -> &[Occurrence] {
        &self.occs
    }
    fn files(&self) -> Vec<&str> {
        vec!["m.py"]
    }
}

fn occ(symbol: &str, r: Range, is_def: bool, enclosing: Option<Range>) -> Occurrence {
    Occurrence { symbol: symbol.to_string(), range: r, is_definition: is_def, enclosing_range: enclosing }
}

fn def(symbol: &str, kind: SymbolKind, name: &str) -> Definition {
    Definition {
        symbol: symbol.to_string(),
        file: Some("m.py".to_string()),
        range: None,
        enclosing_range: None,
        kind,
        display_name: name.to_string(),
        documentation: vec![],
    }
}

#[test]
fn renames_free_function_with_references_and_skips_methods() {
    let dir = std::env::temp_dir().join(format!("slop-rename-e2e-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let src = "def fetchData(x):\n    return x + 1\n\n\ndef caller():\n    return fetchData(3)\n\n\nclass Widget:\n    def renderHTML(self):\n        return \"x\"\n";
    std::fs::write(dir.join("m.py"), src).unwrap();

    let module = format!("{PKG} `m`/__init__:");
    let fetch = format!("{PKG} `m`/fetchData().");
    let caller = format!("{PKG} `m`/caller().");
    let widget = format!("{PKG} `m`/Widget#");
    let render = format!("{PKG} `m`/Widget#renderHTML().");

    let occs = vec![
        occ(&module, range(0, 0, 0, 1), true, None),
        occ(&fetch, range(0, 4, 0, 13), true, Some(range(0, 0, 1, 16))),
        occ(&caller, range(4, 4, 4, 10), true, Some(range(4, 0, 5, 23))),
        occ(&fetch, range(5, 11, 5, 20), false, None), // the call site
        occ(&widget, range(8, 6, 8, 12), true, Some(range(8, 0, 10, 18))),
        occ(&render, range(9, 8, 9, 18), true, Some(range(9, 4, 10, 18))),
    ];
    let defs = [
        def(&module, SymbolKind::Module, "m"),
        def(&fetch, SymbolKind::Function, "fetchData"),
        def(&caller, SymbolKind::Function, "caller"),
        def(&widget, SymbolKind::Class, "Widget"),
        def(&render, SymbolKind::Function, "renderHTML"),
    ]
    .into_iter()
    .map(|d| (d.symbol.clone(), d))
    .collect();
    let resolver = MockResolver { occs, defs };
    let built = build::build_graph(&resolver);

    let finding = |entity: &str| Finding {
        rule: "naming-convention",
        severity: Severity::Advisory,
        entity: entity.to_string(),
        file: "m.py".to_string(),
        lines: (0, 0),
        related: Vec::new(),
        message: String::new(),
        fix_guidance: String::new(),
    };
    let findings = vec![finding("m::fetchData"), finding("m::Widget::renderHTML")];

    let result = rename::plan_renames(&built, &resolver, &dir, &findings);

    // The method is skipped; the free function is planned.
    let planned: Vec<_> = result
        .outcomes
        .iter()
        .filter_map(|o| match o {
            RenameOutcome::Planned(p) => Some(p),
            _ => None,
        })
        .collect();
    assert_eq!(planned.len(), 1, "only the free function should be planned");
    let p = planned[0];
    assert_eq!(p.entity, "m::fetchData");
    assert_eq!(p.new_name, "fetch_data");
    assert_eq!(p.occurrences, 2, "definition + one call site");

    assert!(result.outcomes.iter().any(|o| matches!(
        o,
        RenameOutcome::Skipped { entity, reason }
            if entity == "m::Widget::renderHTML" && reason.contains("method")
    )));

    // The rewritten source renames both the definition and the call, and
    // still parses; the class method is untouched.
    let out = &result.files["m.py"];
    assert!(out.contains("def fetch_data(x):"), "{out}");
    assert!(out.contains("return fetch_data(3)"), "{out}");
    assert!(!out.contains("fetchData"), "{out}");
    assert!(out.contains("def renderHTML(self):"), "method untouched: {out}");

    let _ = std::fs::remove_dir_all(&dir);
}
