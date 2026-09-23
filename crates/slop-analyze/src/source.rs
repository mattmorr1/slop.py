//! Source-level facts: parse every indexed file with slop-parse and join
//! function facts back to graph entities by (file, def line).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

use petgraph::graph::NodeIndex;
use slop_parse::{FunctionFacts, Language};

use crate::build::BuiltGraph;

pub struct FileFacts {
    pub file: String,
    pub functions: Vec<FunctionFacts>,
}

/// Parse each indexed file under `repo_root`. Files that are missing on
/// disk (stale index) or fail to parse are skipped — the graph detectors
/// still cover them.
pub fn parse_repo(repo_root: &Path, files: &[&str]) -> Vec<FileFacts> {
    let mut all = Vec::new();
    for &file in files {
        // No parser for this language => no parser-based facts (graph/effect
        // detectors still cover the file via SCIP).
        let Some(lang) = Language::from_path(file) else {
            continue;
        };
        let Ok(source) = std::fs::read_to_string(repo_root.join(file)) else {
            continue;
        };
        let Ok(functions) = lang.parse(&source) else {
            continue;
        };
        all.push(FileFacts {
            file: file.to_string(),
            functions,
        });
    }
    all
}

pub fn parse_corpus(sources: &BTreeMap<String, Arc<str>>) -> Vec<FileFacts> {
    let files: Vec<_> = sources.iter().collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let workers = std::thread::available_parallelism().map_or(1, usize::from).min(files.len().max(1));
    // Files are independent and pulled from a shared counter (sizes are skewed);
    // re-sorting by index keeps the output deterministic.
    let mut parsed: Vec<(usize, FileFacts)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut out = Vec::new();
                    loop {
                        let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some((file, source)) = files.get(index) else { return out };
                        let Some(language) = Language::from_path(file) else { continue };
                        if let Ok(functions) = language.parse(source) {
                            out.push((index, FileFacts { file: (*file).clone(), functions }));
                        }
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("parser thread panicked"))
            .collect()
    });
    parsed.sort_unstable_by_key(|(index, _)| *index);
    parsed.into_iter().map(|(_, facts)| facts).collect()
}

/// Is `entity_id` defined inside a test module? Rust (and Go) keep tests in the
/// source file under `mod tests`, so [`is_test_file`] — which matches Python's
/// separate-file convention — never sees them. Segment match on the `::` path,
/// so `contests::run` is not mistaken for a test.
pub fn is_test_entity(entity_id: &str) -> bool {
    entity_id.split("::").any(|s| s == "tests" || s == "test")
}

/// Is `path` a test file? Test doubles, mocks, and fixtures are called
/// dynamically by the test framework (collection, dependency injection), which
/// SCIP can't resolve — so `dead-island` there is almost always a false
/// positive. Covers pytest/unittest (`test_x.py`, `x_test.py`, `conftest.py`),
/// the JS/TS `x.test.ts` / `x.spec.js` convention, and Rust's `tests/` dir.
pub fn is_test_file(path: &str) -> bool {
    let p = path.replace('\\', "/");
    let base = p.rsplit('/').next().unwrap_or(&p);
    let stem = base.split('.').next().unwrap_or(base);
    p.starts_with("tests/")
        || p.starts_with("test/")
        || p.contains("/tests/")
        || p.contains("/test/")
        || base.starts_with("test_")
        || stem.ends_with("_test")
        || base.contains(".test.")
        || base.contains(".spec.")
        || base == "conftest.py"
}

/// A dotted module-ish label for a file, used when a fact has no graph entity
/// to name it. Strips the source extension so a `.ts` file doesn't render as
/// `src.foo.ts::bar`.
pub fn module_label(file: &str) -> String {
    let stem = slop_parse::SOURCE_EXTS
        .iter()
        .find_map(|e| file.strip_suffix(&format!(".{e}")))
        .unwrap_or(file);
    stem.replace('/', ".")
}

/// Repo-relative paths of git submodules, parsed from `<repo>/.gitmodules`.
/// Submodule code is a separate project vendored in — findings there aren't the
/// parent repo's to fix, so the audit excludes them.
pub fn submodule_paths(repo: &Path) -> Vec<String> {
    let text = match std::fs::read_to_string(repo.join(".gitmodules")) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("path"))
        .filter_map(|rest| rest.trim().strip_prefix('='))
        .map(|p| p.trim().trim_end_matches('/').to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// Is `file` inside one of the given submodule paths?
pub fn in_submodule(file: &str, submodules: &[String]) -> bool {
    submodules
        .iter()
        .any(|s| file == s || file.starts_with(&format!("{s}/")))
}

/// Join key: a graph Function node whose body span starts on the fact's
/// def line (SCIP enclosing_range start == ruff def start).
pub fn entity_for<'a>(
    built: &'a BuiltGraph,
    by_location: &HashMap<(String, usize), NodeIndex>,
    file: &str,
    fact: &FunctionFacts,
) -> Option<&'a slop_graph::CodeEntity> {
    by_location
        .get(&(file.to_string(), fact.start_line as usize))
        .or_else(|| by_location.get(&(file.to_string(), fact.name_line as usize)))
        .map(|&idx| built.graph.entity(idx))
}

/// Index graph function nodes by (file, body start line).
pub fn location_index(built: &BuiltGraph) -> HashMap<(String, usize), NodeIndex> {
    let mut map = HashMap::new();
    for (idx, entity) in built.graph.entities() {
        if entity.entity_type == slop_graph::NodeType::Function {
            map.insert((entity.file.clone(), entity.source_range.0), idx);
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::{in_submodule, is_test_entity, is_test_file};

    #[test]
    fn test_entities_are_matched_by_path_segment() {
        // Rust/Go keep tests inside the source file, which is_test_file misses.
        assert!(is_test_entity("slop-tui::app::tests::app_with"));
        assert!(is_test_entity("mod::test::helper"));
        // Substring matches must not count.
        assert!(!is_test_entity("services::contests::run"));
        assert!(!is_test_entity("utils::latest::value"));
        assert!(!is_test_entity("slop-analyze::check::load_analysis"));
    }

    #[test]
    fn submodule_containment() {
        let subs = vec!["deeptempo-core".to_string(), "vendor/lib".to_string()];
        assert!(in_submodule("deeptempo-core", &subs));
        assert!(in_submodule("deeptempo-core/pkg/a.py", &subs));
        assert!(in_submodule("vendor/lib/x.py", &subs));
        assert!(!in_submodule("backend/api.py", &subs));
        assert!(!in_submodule("deeptempo-core-extra/a.py", &subs)); // prefix, not path
    }

    #[test]
    fn recognizes_test_paths() {
        assert!(is_test_file("tests/unit/test_foo.py"));
        assert!(is_test_file("backend/tests/test_api.py"));
        assert!(is_test_file("conftest.py"));
        assert!(is_test_file("pkg/foo_test.py"));
        assert!(is_test_file("test_thing.py"));
        // Not tests:
        assert!(!is_test_file("backend/api/case_metrics.py"));
        assert!(!is_test_file("services/latest.py")); // 'test' not a path segment
        assert!(!is_test_file("contest.py"));
    }
}
