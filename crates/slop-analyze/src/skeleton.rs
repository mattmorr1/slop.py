//! Skeleton generation for the context envelope (D11): a deterministic,
//! effect-typed contract — signature + effect signature + docstring —
//! standing in for a function/class body outside the edit zone.

use slop_graph::{CodeEntity, EffectSet, NodeType};

/// Full-fidelity noise strip: drop full-line comments and collapse runs of
/// blank lines to one. Deterministic and reversible-enough for the edit
/// zone (never touches code tokens, so it can't change semantics).
pub fn strip_noise(source: &str) -> String {
    let mut out = String::new();
    let mut blank_run = 0u32;
    for line in source.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
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

fn format_effects(set: &EffectSet) -> String {
    set.0
        .iter()
        .map(|e| format!("{e:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The skeleton block for one entity: effect comment (if any), header
/// (verbatim signature when the caller has one, else a synthesized
/// `def name(...):`), docstring, and a `...` body placeholder.
pub fn skeleton_for(entity: &CodeEntity, signature: Option<&str>) -> String {
    let header = match entity.entity_type {
        NodeType::Class => format!("class {}:", entity.name),
        NodeType::Function => match signature {
            Some(sig) => sig.trim_end().to_string(),
            None => format!("def {}(...):", entity.name),
        },
        _ => return String::new(),
    };

    let mut out = String::new();
    if !entity.effect_signature.is_pure() {
        out.push_str("# effects: ");
        out.push_str(&format_effects(&entity.effect_signature));
        out.push('\n');
    }
    out.push_str(&header);
    out.push('\n');
    if let Some(doc) = entity.docstring.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        out.push_str("    \"\"\"");
        out.push_str(doc);
        out.push_str("\"\"\"\n");
    }
    out.push_str("    ...\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use slop_graph::Effect;

    fn entity(entity_type: NodeType, name: &str) -> CodeEntity {
        CodeEntity {
            id: format!("m::{name}"),
            entity_type,
            name: name.into(),
            signature: String::new(),
            docstring: None,
            file: "m.py".into(),
            source_range: (0, 0),
            body_hash: String::new(),
            effect_signature: EffectSet::pure(),
        }
    }

    #[test]
    fn pure_function_skeleton_has_no_effect_comment() {
        let e = entity(NodeType::Function, "parse_date");
        let sk = skeleton_for(&e, Some("def parse_date(value: str) -> date:"));
        assert!(!sk.contains("# effects"));
        assert!(sk.contains("def parse_date(value: str) -> date:"));
        assert!(sk.contains("...\n"));
    }

    #[test]
    fn effectful_function_skeleton_reports_effects() {
        let mut e = entity(NodeType::Function, "fetch");
        e.effect_signature.insert(Effect::Net);
        e.docstring = Some("Fetch a resource.".into());
        let sk = skeleton_for(&e, Some("def fetch(url: str) -> bytes:"));
        assert!(sk.starts_with("# effects: Net\n"));
        assert!(sk.contains("\"\"\"Fetch a resource.\"\"\""));
    }

    #[test]
    fn missing_signature_falls_back_to_synthesized_header() {
        let e = entity(NodeType::Function, "mystery");
        let sk = skeleton_for(&e, None);
        assert!(sk.contains("def mystery(...):"));
    }

    #[test]
    fn class_skeleton_uses_class_header() {
        let e = entity(NodeType::Class, "Widget");
        let sk = skeleton_for(&e, None);
        assert!(sk.starts_with("class Widget:\n"));
    }

    #[test]
    fn strip_noise_drops_full_line_comments_and_collapses_blanks() {
        let src = "def f():\n    # explain\n    x = 1\n\n\n\n    return x\n";
        let stripped = strip_noise(src);
        assert!(!stripped.contains('#'));
        assert!(!stripped.contains("\n\n\n"));
        assert!(stripped.contains("x = 1"));
    }

    #[test]
    fn strip_noise_preserves_inline_trailing_comments() {
        // Only full-line comments are stripped — trailing `#` could be
        // inside a string literal, so it's left alone rather than guessed at.
        let src = "x = 1  # not a full-line comment\n";
        assert_eq!(strip_noise(src), src);
    }
}
