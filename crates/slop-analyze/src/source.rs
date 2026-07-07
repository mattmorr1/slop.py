//! Source-level facts: parse every indexed file with slop-parse and join
//! function facts back to graph entities by (file, def line).

use std::collections::HashMap;
use std::path::Path;

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

/// Is `path` a test file? Test doubles, mocks, and fixtures are called
/// dynamically by the test framework (collection, dependency injection), which
/// SCIP can't resolve — so `dead-island` there is almost always a false
/// positive. Matches the near-universal Python conventions (pytest/unittest).
pub fn is_test_file(path: &str) -> bool {
    let p = path.replace('\\', "/");
    let base = p.rsplit('/').next().unwrap_or(&p);
    p.starts_with("tests/")
        || p.starts_with("test/")
        || p.contains("/tests/")
        || p.contains("/test/")
        || base.starts_with("test_")
        || base.ends_with("_test.py")
        || base == "conftest.py"
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
    use super::is_test_file;

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
