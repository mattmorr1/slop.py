//! Skeleton generation for the context envelope (D11): a deterministic,
//! effect-typed contract — signature + effect signature + docstring —
//! standing in for a function/class body outside the edit zone.
//!
//! Everything here emits *source syntax*, so it is language-aware: a Python
//! `#` comment stripper run over Rust deletes `#[derive]`, and a `"""doc"""`
//! body in a `.ts` file is not a contract, it's a syntax error.

use slop_graph::{CodeEntity, EffectSet, NodeType};
use slop_parse::Language;

fn comment_syntax(lang: Option<Language>) -> (Option<&'static str>, &'static [&'static str]) {
    match lang.map(Language::line_comment) {
        Some((prefix, keep)) => (Some(prefix), keep),
        None => (None, &[]),
    }
}

/// Full-fidelity noise strip: drop full-line comments and collapse runs of
/// blank lines to one. Deterministic and reversible-enough for the edit
/// zone (never touches code tokens, so it can't change semantics). An
/// unrecognized language keeps every comment — guessing the syntax is how
/// you delete an attribute.
pub fn strip_noise(source: &str, lang: Option<Language>) -> String {
    let (prefix, keep) = comment_syntax(lang);
    let mut out = String::new();
    let mut blank_run = 0u32;
    for line in source.lines() {
        let trimmed = line.trim_start();
        if prefix.is_some_and(|p| trimmed.starts_with(p)) && !keep.iter().any(|k| trimmed.starts_with(k)) {
            continue;
        }
        if trimmed.is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// The comment prefix skeleton annotations (`effects:`, `summary:`) use.
pub fn annotation_prefix(lang: Option<Language>) -> &'static str {
    comment_syntax(lang).0.unwrap_or("#")
}

fn format_effects(set: &EffectSet) -> String {
    set.0
        .iter()
        .map(|e| format!("{e:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The docstring rendered where the language puts it: inside the body for
/// Python, above the item everywhere else. Returns `(before_header, in_body)`.
fn render_doc(doc: &str, lang: Option<Language>) -> (String, String) {
    match lang {
        Some(Language::Rust) => (
            doc.lines().map(|l| format!("/// {l}\n")).collect(),
            String::new(),
        ),
        Some(Language::JavaScript | Language::TypeScript | Language::Tsx) => {
            (format!("/** {doc} */\n"), String::new())
        }
        _ => (String::new(), format!("    \"\"\"{doc}\"\"\"\n")),
    }
}

/// The skeleton block for one entity: effect comment (if any), header
/// (verbatim signature when the caller has one, else a synthesized one) and a
/// body placeholder. Empty when no honest header can be produced — a skeleton
/// in the wrong language is worse than no skeleton.
pub fn skeleton_for(entity: &CodeEntity, signature: Option<&str>) -> String {
    let lang = Language::from_path(&entity.file);
    let python = matches!(lang, Some(Language::Python) | None);
    let header = match (entity.entity_type, signature) {
        (NodeType::Function | NodeType::Class, Some(sig)) => sig.trim_end().to_string(),
        // Only Python's declaration syntax is recoverable from a name alone;
        // elsewhere a bare name could be a struct, trait, enum or impl.
        (NodeType::Class, None) if python => format!("class {}:", entity.name),
        (NodeType::Function, None) if python => format!("def {}(...):", entity.name),
        _ => return String::new(),
    };

    let mut out = String::new();
    if !entity.effect_signature.is_pure() {
        out.push_str(annotation_prefix(lang));
        out.push_str(" effects: ");
        out.push_str(&format_effects(&entity.effect_signature));
        out.push('\n');
    }
    let doc = entity.docstring.as_deref().map(str::trim).filter(|d| !d.is_empty());
    let (before, in_body) = doc.map(|d| render_doc(d, lang)).unwrap_or_default();
    out.push_str(&before);
    out.push_str(&header);
    if python {
        out.push('\n');
        out.push_str(&in_body);
        out.push_str("    ...\n");
    } else {
        out.push_str(" { /* ... */ }\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use slop_graph::Effect;

    fn entity(entity_type: NodeType, name: &str, file: &str) -> CodeEntity {
        CodeEntity {
            id: format!("m::{name}"),
            entity_type,
            name: name.into(),
            signature: String::new(),
            docstring: None,
            file: file.into(),
            source_range: (0, 0),
            body_hash: String::new(),
            effect_signature: EffectSet::pure(),
        }
    }

    #[test]
    fn pure_function_skeleton_has_no_effect_comment() {
        let e = entity(NodeType::Function, "parse_date", "m.py");
        let sk = skeleton_for(&e, Some("def parse_date(value: str) -> date:"));
        assert!(!sk.contains("# effects"));
        assert!(sk.contains("def parse_date(value: str) -> date:"));
        assert!(sk.contains("...\n"));
    }

    #[test]
    fn effectful_function_skeleton_reports_effects() {
        let mut e = entity(NodeType::Function, "fetch", "m.py");
        e.effect_signature.insert(Effect::Net);
        e.docstring = Some("Fetch a resource.".into());
        let sk = skeleton_for(&e, Some("def fetch(url: str) -> bytes:"));
        assert!(sk.starts_with("# effects: Net\n"));
        assert!(sk.contains("\"\"\"Fetch a resource.\"\"\""));
    }

    #[test]
    fn missing_signature_falls_back_to_synthesized_header() {
        let e = entity(NodeType::Function, "mystery", "m.py");
        let sk = skeleton_for(&e, None);
        assert!(sk.contains("def mystery(...):"));
    }

    #[test]
    fn class_skeleton_uses_class_header() {
        let e = entity(NodeType::Class, "Widget", "m.py");
        let sk = skeleton_for(&e, None);
        assert!(sk.starts_with("class Widget:\n"));
    }

    #[test]
    fn rust_skeleton_uses_rust_syntax() {
        let mut e = entity(NodeType::Function, "fetch", "src/m.rs");
        e.effect_signature.insert(Effect::Net);
        e.docstring = Some("Fetch a resource.".into());
        let sk = skeleton_for(&e, Some("pub fn fetch(url: &str) -> Vec<u8>"));
        assert!(sk.starts_with("// effects: Net\n"));
        assert!(sk.contains("/// Fetch a resource.\n"));
        assert!(sk.contains("pub fn fetch(url: &str) -> Vec<u8> { /* ... */ }"));
        assert!(!sk.contains("\"\"\""));
    }

    #[test]
    fn non_python_entity_without_a_signature_is_skipped() {
        // `class Widget:` in a .rs file is not a contract, it's noise.
        let e = entity(NodeType::Class, "Widget", "src/m.rs");
        assert!(skeleton_for(&e, None).is_empty());
    }

    #[test]
    fn strip_noise_drops_full_line_comments_and_collapses_blanks() {
        let src = "def f():\n    # explain\n    x = 1\n\n\n\n    return x\n";
        let stripped = strip_noise(src, Some(Language::Python));
        assert!(!stripped.contains('#'));
        assert!(!stripped.contains("\n\n\n"));
        assert!(stripped.contains("x = 1"));
    }

    #[test]
    fn strip_noise_preserves_inline_trailing_comments() {
        // Only full-line comments are stripped — trailing `#` could be
        // inside a string literal, so it's left alone rather than guessed at.
        let src = "x = 1  # not a full-line comment\n";
        assert_eq!(strip_noise(src, Some(Language::Python)), src);
    }

    #[test]
    fn strip_noise_never_deletes_rust_attributes() {
        let src = "#![allow(dead_code)]\n// explain\n/// contract\n#[derive(Debug)]\nstruct S;\n";
        let stripped = strip_noise(src, Some(Language::Rust));
        assert!(stripped.contains("#![allow(dead_code)]"));
        assert!(stripped.contains("#[derive(Debug)]"));
        assert!(stripped.contains("/// contract"));
        assert!(!stripped.contains("// explain"));
    }

    #[test]
    fn strip_noise_on_an_unknown_language_keeps_everything() {
        let src = "# not necessarily a comment\ncode\n";
        assert_eq!(strip_noise(src, None), src);
    }
}
