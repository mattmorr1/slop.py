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

/// Importable-module identity of an **external** symbol, for effect-seed
/// matching. scip-python encodes the module in the descriptor (so `entity_id`
/// already yields `requests.api.post`); scip-typescript instead puts the
/// importable name in the *package* field (`axios`, `pg`) or a quoted
/// module-specifier descriptor (`"node:fs"`) for builtins, leaving the useless
/// `.d.ts` filename as the descriptor's "module". This derives a clean
/// `<module>.<member>` (e.g. `axios.get`, `fs.readFileSync`, `process.env`) so
/// the seed table matches on the name you'd actually `import`.
pub fn external_effect_id(symbol: &str) -> Option<String> {
    let mut fields = symbol.splitn(5, ' ');
    let scheme = fields.next()?;
    let _manager = fields.next()?;
    let package = fields.next()?;
    let _version = fields.next()?;
    let descriptors = fields.next()?;

    if scheme != "scip-typescript" {
        return entity_id(symbol); // python (and anything descriptor-encoded)
    }

    let segments = split_descriptors(descriptors);
    // Module: a quoted specifier (`"node:fs"` -> `fs`) wins; else the npm
    // package, unless it's a type-only / stdlib package with no import identity.
    let specifier = segments.iter().find_map(|s| {
        let inner = s.trim_matches('`');
        let unquoted = inner.strip_prefix('"').and_then(|x| x.strip_suffix('"'))?;
        Some(unquoted.strip_prefix("node:").unwrap_or(unquoted).to_string())
    });
    let module = specifier.or_else(|| {
        (!package.starts_with("@types/") && package != "typescript")
            .then(|| package.to_string())
    })?;
    // Member: the last real descriptor (method/property), skipping the `.d.ts`
    // file segment and the quoted specifier.
    let member = segments.iter().rev().find_map(|s| {
        let c = s
            .trim_matches('`')
            .trim_end_matches("().")
            .trim_end_matches(['#', ':', '.', '!']);
        (!c.is_empty() && !c.ends_with(".d.ts") && !c.starts_with('"')).then(|| c.to_string())
    });
    Some(match member {
        Some(m) if m != module => format!("{module}::{m}"),
        _ => module,
    })
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

    #[test]
    fn external_effect_id_normalizes_typescript() {
        // npm package: identity is the package name + member, not the .d.ts file.
        assert_eq!(
            external_effect_id("scip-typescript npm axios 1.18.1 `index.d.ts`/Axios#get().").as_deref(),
            Some("axios::get")
        );
        // node builtin: identity is the module specifier, not `fs.d.ts`.
        assert_eq!(
            external_effect_id("scip-typescript npm @types/node 26.1.0 `fs.d.ts`/`\"node:fs\"`/readFileSync().").as_deref(),
            Some("fs::readFileSync")
        );
        // process.env — the case that motivated the member-tail rule.
        assert_eq!(
            external_effect_id("scip-typescript npm @types/node 26.1.0 `process.d.ts`/`\"node:process\"`/global/NodeJS/Process#env.").as_deref(),
            Some("process::env")
        );
        // type-only / stdlib packages carry no import identity.
        assert_eq!(
            external_effect_id("scip-typescript npm typescript 5.9.3 lib/`lib.es5.d.ts`/Promise#"),
            None
        );
        // Python is unchanged (descriptor-encoded module).
        assert_eq!(
            external_effect_id("scip-python python requests 2.0 `requests.api`/post().").as_deref(),
            Some("requests.api::post")
        );
    }
}
