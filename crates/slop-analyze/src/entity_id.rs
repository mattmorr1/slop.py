//! Stable, human-readable entity IDs derived from SCIP symbols.
//!
//! `scip-python python toy-repo <ver> `core.http_client`/HttpClient#get().`
//! becomes `core.http_client::HttpClient::get`. IDs are what policies match
//! against and what findings display — they must not embed the indexer
//! version or commit hash.

/// Extract the dotted module path a symbol belongs to, e.g.
/// `core.http_client` or `urllib.request` for stdlib refs.
pub fn module_of(symbol: &str) -> Option<String> {
    let descriptors = descriptors_of(symbol)?;
    if let Some(rest) = descriptors.strip_prefix('`') {
        let end = rest.find('`')?;
        return Some(rest[..end].to_string());
    }
    // Unquoted namespace descriptors: take leading `name:`-style segments.
    let first = descriptors.split('/').next()?;
    first.strip_suffix(':').map(|s| s.to_string())
}

/// Human-oriented entity ID: module path joined to the descriptor chain
/// with `::`, suffix punctuation stripped.
pub fn entity_id(symbol: &str) -> Option<String> {
    let descriptors = descriptors_of(symbol)?;
    let mut parts: Vec<String> = Vec::new();
    for segment in split_descriptors(descriptors) {
        let cleaned = segment
            .trim_matches('`')
            .trim_end_matches("().")
            .trim_end_matches(['#', ':', '.', '!'])
            .to_string();
        // `__init__:` is the module-file marker and folds into the module
        // path; `__init__().` is a real constructor and must stay distinct.
        if cleaned.is_empty() || (cleaned == "__init__" && segment.ends_with(':')) {
            continue;
        }
        parts.push(cleaned);
    }
    if parts.is_empty() {
        // Module symbols reduce to just their module path.
        return module_of(symbol);
    }
    let module = module_of(symbol);
    match module {
        Some(m) if parts.first() != Some(&m) => Some(format!("{m}::{}", parts.join("::"))),
        _ => Some(parts.join("::")),
    }
}

/// The descriptor tail of a SCIP symbol: everything after the 4
/// space-separated header fields (scheme, manager, package name, version).
fn descriptors_of(symbol: &str) -> Option<&str> {
    if symbol.starts_with("local ") {
        return None;
    }
    let mut rest = symbol;
    for _ in 0..4 {
        let idx = rest.find(' ')?;
        rest = &rest[idx + 1..];
    }
    Some(rest)
}

/// Split a descriptor chain into segments. Backtick-quoted names may
/// contain `/` and `#`, so honor quoting.
fn split_descriptors(descriptors: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_quote = false;
    let bytes = descriptors.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'`' => in_quote = !in_quote,
            b'/' | b'#' if !in_quote => {
                if i > start {
                    parts.push(&descriptors[start..i]);
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < descriptors.len() {
        parts.push(&descriptors[start..]);
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    const METHOD: &str =
        "scip-python python toy-repo abc123 `core.http_client`/HttpClient#get().";
    const MODULE: &str = "scip-python python python-stdlib 3.11 `urllib.request`/__init__:";
    const FUNC: &str = "scip-python python toy-repo abc123 `utils.dates`/parse_date().";

    #[test]
    fn extracts_module() {
        assert_eq!(module_of(METHOD).as_deref(), Some("core.http_client"));
        assert_eq!(module_of(MODULE).as_deref(), Some("urllib.request"));
    }

    #[test]
    fn builds_entity_ids() {
        assert_eq!(
            entity_id(METHOD).as_deref(),
            Some("core.http_client::HttpClient::get")
        );
        assert_eq!(entity_id(MODULE).as_deref(), Some("urllib.request"));
        assert_eq!(entity_id(FUNC).as_deref(), Some("utils.dates::parse_date"));
    }

    #[test]
    fn locals_have_no_id() {
        assert_eq!(entity_id("local 3"), None);
        assert_eq!(module_of("local 3"), None);
    }
}
