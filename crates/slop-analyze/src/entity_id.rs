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
    if let Some((package, descriptors)) = rust_parts(symbol) {
        return Some(rust_entity_id(package, descriptors));
    }
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
/// the seed table matches on the name you'd actually `import`. Rust falls
/// through to [`entity_id`], which already crate-prefixes (`std::env::var`).
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

/// Package (crate) and descriptor fields of a `rust-analyzer` SCIP symbol, or
/// `None` for any other scheme. Rust needs both: the crate lives in the package
/// field (like scip-typescript) *and* the module path in the descriptors (like
/// scip-python).
fn rust_parts(symbol: &str) -> Option<(&str, &str)> {
    let mut fields = symbol.splitn(5, ' ');
    if fields.next()? != "rust-analyzer" {
        return None;
    }
    let _manager = fields.next()?;
    let package = fields.next()?;
    let _version = fields.next()?;
    Some((package, fields.next()?))
}

/// Flatten a Rust descriptor chain to `crate::module::Type::method`. Rust
/// descriptors carry a `crate/` root marker, anonymous `impl#` blocks, generic
/// parameters and trait qualifiers — e.g. ``fs/impl#[DirEntry]metadata().`` and
/// ``collections/hash/map/impl#[`HashMap<K, V, S, A>`][`Index<&Q>`]index().`` —
/// none of which belong in the ID the seed table matches and findings display.
fn rust_entity_id(package: &str, descriptors: &str) -> String {
    let mut parts = vec![package.to_string()];
    for segment in split_descriptors(descriptors) {
        // `crate` is the root marker and `impl` an anonymous block: neither names anything.
        if segment == "crate" || segment == "impl" {
            continue;
        }
        let (groups, member) = bracket_groups(segment);
        // First bracket group = the implementing type; later groups are trait
        // qualifiers that add nothing to the entity's identity.
        parts.extend(groups.first().map(|g| clean_rust_name(g)));
        parts.push(clean_rust_name(member));
    }
    parts.retain(|p| !p.is_empty());
    parts.join("::")
}

/// Split a Rust descriptor segment into its top-level `[...]` group contents
/// and the trailing member text. Backtick-quoted type names may themselves
/// contain brackets (`[`[u8; 4]`]`), so quoting and nesting are both honored.
fn bracket_groups(segment: &str) -> (Vec<&str>, &str) {
    let mut groups = Vec::new();
    let mut in_quote = false;
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut tail = 0usize;
    for (i, &b) in segment.as_bytes().iter().enumerate() {
        match b {
            b'`' => in_quote = !in_quote,
            b'[' if !in_quote => {
                if depth == 0 {
                    start = i + 1;
                }
                depth += 1;
            }
            b']' if !in_quote => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    groups.push(&segment[start..i]);
                    tail = i + 1;
                }
            }
            _ => {}
        }
    }
    (groups, &segment[tail..])
}

/// Strip backtick quoting, SCIP suffix punctuation and generic parameters from
/// a Rust descriptor name: ``` `HashMap<K, V>` ``` becomes `HashMap`.
fn clean_rust_name(name: &str) -> String {
    let base = name.trim().trim_matches('`');
    let base = base.trim_end_matches("().").trim_end_matches(['#', ':', '.', '!', '/']);
    match base.find('<') {
        Some(i) => base[..i].trim_end().to_string(),
        None => base.trim().to_string(),
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

    // Every input below is a verbatim symbol from a real `rust-analyzer scip`
    // index of this repo — not a hand-written guess at the grammar.
    const STD: &str = "rust-analyzer cargo std https://github.com/rust-lang/rust/library/std ";

    #[test]
    fn rust_ids_are_crate_prefixed() {
        // Internal crate entities: the crate disambiguates same-named modules.
        assert_eq!(
            entity_id("rust-analyzer cargo slop-analyze 0.0.1 findings/Finding#").as_deref(),
            Some("slop-analyze::findings::Finding")
        );
        // The `crate/` root marker reduces to the crate itself.
        assert_eq!(
            entity_id("rust-analyzer cargo slop-analyze 0.0.1 crate/").as_deref(),
            Some("slop-analyze")
        );
        // Free function in a std module — the shape effect seeds key off.
        assert_eq!(entity_id(&format!("{STD}env/var().")).as_deref(), Some("std::env::var"));
        assert_eq!(
            entity_id(&format!("{STD}fs/create_dir_all().")).as_deref(),
            Some("std::fs::create_dir_all")
        );
    }

    #[test]
    fn rust_impl_blocks_and_generics_collapse() {
        // `impl#` is anonymous; the implementing type is what names the method.
        assert_eq!(
            entity_id(&format!("{STD}fs/impl#[DirEntry]metadata().")).as_deref(),
            Some("std::fs::DirEntry::metadata")
        );
        // Generic parameters are stripped; the trait qualifier group is dropped.
        assert_eq!(
            entity_id(&format!("{STD}collections/hash/map/impl#[`HashMap<K, V, S, A>`]contains_key().")).as_deref(),
            Some("std::collections::hash::map::HashMap::contains_key")
        );
        assert_eq!(
            entity_id(&format!("{STD}collections/hash/map/impl#[`HashMap<K, V, S, A>`][`Index<&Q>`]index().")).as_deref(),
            Some("std::collections::hash::map::HashMap::index")
        );
    }

    #[test]
    fn rust_seed_prefixes_match_dotted_ids() {
        // What `infer_effects` actually feeds the seed table (it maps `::`->`.`).
        let dotted = |sym: &str| entity_id(sym).unwrap().replace("::", ".");
        assert!(dotted(&format!("{STD}env/var().")).starts_with("std.env.var"));
        assert!(dotted(&format!("{STD}fs/impl#[OpenOptions]create().")).starts_with("std.fs."));
        assert!(dotted(&format!("{STD}process/impl#[Command]spawn().")).starts_with("std.process.Command"));
    }
}
