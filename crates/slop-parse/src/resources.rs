//! Which *named resource* an effect acquisition touches — currently the
//! environment variable an `Env` effect reads.
//!
//! The effect lattice says a function reads the environment; it does not say
//! *what* it reads, so two functions both pulling `DATABASE_URL` out of the
//! environment in different modules look identical to one that reads
//! `TZ`. The name is what makes config sprawl visible.
//!
//! Env is the tractable resource. The variable name sits as a literal at the
//! acquisition site in every supported language, so recall is high. A `Net`
//! host is usually assembled (`f"{base}/path"`) rather than written literally,
//! and a `Db` table hides inside SQL — both would report a fraction of reality.
//! Note also that the base URL a `Net` call uses is itself typically read from
//! the environment, so this is the substrate for host identity later.

use ruff_python_ast::token::TokenKind;
use ruff_python_parser::parse_module;
use ruff_text_size::Ranged;
use tree_sitter::{Node, Parser};

use crate::Language;

/// One environment variable read, with the 0-based line it happens on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvRead {
    pub var: String,
    pub line: u32,
}

/// Accessor expressions that read the environment, per language. Matched as a
/// suffix of the callee/subscript text so `os.environ`, `environ` and
/// `std::env::var` all resolve without enumerating every import alias.
const PY_ACCESSORS: &[&str] =
    &["os.environ", "os.getenv", "environ.get", "environ", "getenv"];
const RS_ACCESSORS: &[&str] = &["env::var", "env::var_os", "std::env::var"];
const JS_ACCESSORS: &[&str] = &["process.env"];

/// Every environment variable `source` reads. Empty when the source doesn't
/// parse — a caller that cannot read the file must not guess at its config.
pub fn env_reads(lang: Language, source: &str) -> Vec<EnvRead> {
    let mut out = match lang {
        Language::Python => python_env_reads(source),
        Language::Rust => ts_env_reads(source, tree_sitter_rust::LANGUAGE.into(), RS_ACCESSORS),
        Language::JavaScript => {
            ts_env_reads(source, tree_sitter_javascript::LANGUAGE.into(), JS_ACCESSORS)
        }
        Language::TypeScript => ts_env_reads(
            source,
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            JS_ACCESSORS,
        ),
    };
    out.sort_by(|a, b| a.var.cmp(&b.var).then(a.line.cmp(&b.line)));
    out.dedup();
    out
}

/// Does `text` name an env accessor? Suffix match on a segment boundary, so
/// `my_os.environ` counts and `zenviron` does not.
fn is_accessor(text: &str, accessors: &[&str]) -> bool {
    accessors.iter().any(|a| {
        text == *a
            || text
                .strip_suffix(a)
                .is_some_and(|p| p.ends_with('.') || p.ends_with("::"))
    })
}

/// The variable name inside a quoted literal, or `None` when the argument is
/// computed — a name we cannot read is not a name we should report.
fn literal_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    // Skip any prefix (b, r, f, u) and take what's inside matching quotes.
    let body = trimmed.trim_start_matches(|c: char| c.is_ascii_alphabetic());
    let quote = body.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let inner = body.strip_prefix(quote)?.strip_suffix(quote)?;
    // An f-string or concatenation is computed, not a literal name.
    if inner.is_empty() || inner.contains(['{', '"', '\'', '\\']) {
        return None;
    }
    Some(inner.to_string())
}

/// Python: a `Name`/`.` token run that names an accessor, followed by `[` or
/// `(` and a string literal. Token-level rather than AST-level because the
/// pattern is purely local, and the tokens already carry the text and line.
fn python_env_reads(source: &str) -> Vec<EnvRead> {
    let Ok(parsed) = parse_module(source) else {
        return Vec::new();
    };
    let tokens: Vec<_> = parsed
        .tokens()
        .iter()
        .filter(|t| {
            !matches!(
                t.kind(),
                TokenKind::Comment | TokenKind::NonLogicalNewline | TokenKind::Newline
            )
        })
        .collect();
    let line_of = |offset: ruff_text_size::TextSize| {
        source[..usize::from(offset)].bytes().filter(|b| *b == b'\n').count() as u32
    };

    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        // Accumulate a dotted run: Name ('.' Name)*
        if tokens[i].kind() != TokenKind::Name {
            i += 1;
            continue;
        }
        let start = i;
        let mut text = source[tokens[i].range()].to_string();
        while i + 2 < tokens.len()
            && tokens[i + 1].kind() == TokenKind::Dot
            && tokens[i + 2].kind() == TokenKind::Name
        {
            text.push('.');
            text.push_str(&source[tokens[i + 2].range()]);
            i += 2;
        }
        if is_accessor(&text, PY_ACCESSORS)
            && i + 2 < tokens.len()
            && matches!(tokens[i + 1].kind(), TokenKind::Lsqb | TokenKind::Lpar)
            && tokens[i + 2].kind() == TokenKind::String
        {
            if let Some(var) = literal_name(&source[tokens[i + 2].range()]) {
                out.push(EnvRead { var, line: line_of(tokens[start].start()) });
            }
        }
        i += 1;
    }
    out
}

/// Rust / JS / TS via tree-sitter. Two shapes: a call or index whose callee
/// names an accessor and whose first argument is a string literal, and JS's
/// dotted `process.env.NAME`.
fn ts_env_reads(source: &str, language: tree_sitter::Language, accessors: &[&str]) -> Vec<EnvRead> {
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    walk(tree.root_node(), source.as_bytes(), accessors, &mut out);
    out
}

fn walk(node: Node, src: &[u8], accessors: &[&str], out: &mut Vec<EnvRead>) {
    let line = node.start_position().row as u32;
    let text = |n: Node| n.utf8_text(src).unwrap_or("").to_string();

    match node.kind() {
        // `env::var("X")` / `process.env["X"]`
        "call_expression" | "subscript_expression" => {
            let callee = node
                .child_by_field_name("function")
                .or_else(|| node.child_by_field_name("object"));
            if let Some(callee) = callee {
                if is_accessor(text(callee).trim(), accessors) {
                    let arg = node
                        .child_by_field_name("arguments")
                        .and_then(|a| a.named_child(0))
                        .or_else(|| node.child_by_field_name("index"));
                    if let Some(var) = arg.and_then(|a| literal_name(&text(a))) {
                        out.push(EnvRead { var, line });
                    }
                }
            }
        }
        // `process.env.NAME` — the name is an identifier, not a literal.
        "member_expression" => {
            if let (Some(obj), Some(prop)) = (
                node.child_by_field_name("object"),
                node.child_by_field_name("property"),
            ) {
                if is_accessor(text(obj).trim(), accessors) {
                    out.push(EnvRead { var: text(prop), line });
                }
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, src, accessors, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(lang: Language, src: &str) -> Vec<String> {
        env_reads(lang, src).into_iter().map(|r| r.var).collect()
    }

    #[test]
    fn python_subscript_and_getenv() {
        let src = "import os\nKEY = os.environ[\"API_KEY\"]\nH = os.getenv('OLLAMA_HOST')\nD = os.environ.get(\"DEBUG\")\n";
        let got = vars(Language::Python, src);
        assert!(got.contains(&"API_KEY".to_string()), "{got:?}");
        assert!(got.contains(&"OLLAMA_HOST".to_string()), "{got:?}");
        assert!(got.contains(&"DEBUG".to_string()), "{got:?}");
    }

    #[test]
    fn rust_env_var() {
        let src = "fn f() {\n    let k = std::env::var(\"ANTHROPIC_API_KEY\").ok();\n    let h = env::var(\"OLLAMA_HOST\");\n}\n";
        let got = vars(Language::Rust, src);
        assert_eq!(got, vec!["ANTHROPIC_API_KEY", "OLLAMA_HOST"]);
    }

    #[test]
    fn js_dotted_and_indexed() {
        let src = "const a = process.env.NODE_ENV;\nconst b = process.env[\"PORT\"];\n";
        let got = vars(Language::JavaScript, src);
        assert!(got.contains(&"NODE_ENV".to_string()), "{got:?}");
        assert!(got.contains(&"PORT".to_string()), "{got:?}");
    }

    #[test]
    fn a_computed_name_is_not_reported() {
        // We cannot know what this reads, so claiming a name would be a lie.
        let src = "import os\nv = os.environ[f\"PREFIX_{name}\"]\nw = os.environ[key]\n";
        assert!(vars(Language::Python, src).is_empty());
    }

    #[test]
    fn a_name_in_a_comment_or_unrelated_string_is_not_a_read() {
        let src = "import os\n# os.environ[\"FAKE\"]\nmsg = 'os.environ[\"ALSO_FAKE\"]'\n";
        assert!(vars(Language::Python, src).is_empty());
    }

    #[test]
    fn similar_identifiers_do_not_match() {
        let src = "v = my_environment[\"X\"]\nw = zgetenv(\"Y\")\n";
        assert!(vars(Language::Python, src).is_empty());
    }

    #[test]
    fn line_numbers_are_reported_for_location() {
        let src = "import os\n\nK = os.environ[\"API_KEY\"]\n";
        assert_eq!(env_reads(Language::Python, src)[0].line, 2);
    }
}
