//! Qualified-name extraction for the write-time pre-check: every dotted or
//! scoped name a source file *references*, with comments and string literals
//! excluded so a name merely mentioned in prose never reads as an effect
//! acquisition. Names are normalized to `.` separators, which is the form the
//! effect seed table matches.
//!
//! Only structural nodes are collected (Python `Name`/`Dot` token runs, Rust
//! `scoped_identifier`, JS `member_expression`), so comments and strings are
//! excluded by construction rather than by stripping. Module specifiers are the
//! deliberate exception: JS puts them *in* string literals (`require("fs")`), so
//! import sources are read on purpose.

use ruff_python_ast::token::TokenKind;
use ruff_python_parser::parse_module;
use ruff_text_size::Ranged;
use tree_sitter::{Node, Parser};

use crate::Language;

/// Every qualified name `source` references, deduplicated, `.`-separated.
/// Returns empty on a parse failure — a pre-check that cannot read the proposed
/// content must stay silent rather than guess.
pub fn qualified_names(lang: Language, source: &str) -> Vec<String> {
    let mut out = match lang {
        Language::Python => python_names(source),
        Language::Rust => ts_names(source, tree_sitter_rust::LANGUAGE.into(), Syntax::Rust),
        Language::JavaScript => {
            ts_names(source, tree_sitter_javascript::LANGUAGE.into(), Syntax::Js)
        }
        Language::TypeScript => ts_names(
            source,
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Syntax::Js,
        ),
        Language::Tsx => ts_names(source, tree_sitter_typescript::LANGUAGE_TSX.into(), Syntax::Js),
    };
    out.sort();
    out.dedup();
    out
}

/// Python: walk the token stream joining `Name (Dot Name)*` runs. Comment and
/// string tokens are different kinds, so they break a run instead of joining it.
fn python_names(source: &str) -> Vec<String> {
    let Ok(parsed) = parse_module(source) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut chain: Vec<&str> = Vec::new();
    let mut after_dot = false;
    for token in parsed.tokens() {
        match token.kind() {
            TokenKind::Name => {
                let text = &source[token.range()];
                if !after_dot {
                    flush(&mut chain, &mut out);
                }
                chain.push(text);
                after_dot = false;
            }
            // A dot only continues a chain; a leading dot (relative import) does not start one.
            TokenKind::Dot if !chain.is_empty() => after_dot = true,
            _ => {
                flush(&mut chain, &mut out);
                after_dot = false;
            }
        }
    }
    flush(&mut chain, &mut out);
    out
}

fn flush(chain: &mut Vec<&str>, out: &mut Vec<String>) {
    if !chain.is_empty() {
        out.push(chain.join("."));
        chain.clear();
    }
}

/// Which node kinds carry qualified names in a given grammar.
enum Syntax {
    Rust,
    Js,
}

fn ts_names(source: &str, language: tree_sitter::Language, syntax: Syntax) -> Vec<String> {
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    walk(tree.root_node(), source.as_bytes(), &syntax, &mut out);
    out
}

fn walk(node: Node, src: &[u8], syntax: &Syntax, out: &mut Vec<String>) {
    let kind = node.kind();
    let matched = match syntax {
        // `std::fs::read_to_string`, `reqwest::blocking::get`. Method calls on
        // values (`file.write_all`) need type information the pre-check does not
        // have, so they are left to the graph-backed detectors.
        Syntax::Rust => kind == "scoped_identifier",
        Syntax::Js => kind == "member_expression",
    };
    if matched {
        if let Ok(text) = node.utf8_text(src) {
            out.push(text.replace("::", ".").replace(char::is_whitespace, ""));
        }
        // Don't descend: the outermost chain already carries the whole path.
        return;
    }
    // JS module specifiers live in string literals, so they are read explicitly.
    if matches!(syntax, Syntax::Js) {
        if let Some(spec) = js_module_specifier(node, src) {
            out.push(spec);
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, src, syntax, out);
    }
}

/// The imported module of an `import ... from "x"` or `require("x")`, unquoted.
fn js_module_specifier(node: Node, src: &[u8]) -> Option<String> {
    let string_node = match node.kind() {
        "import_statement" => node.child_by_field_name("source")?,
        "call_expression" => {
            let callee = node.child_by_field_name("function")?;
            if callee.utf8_text(src).ok()? != "require" {
                return None;
            }
            let args = node.child_by_field_name("arguments")?;
            let mut cursor = args.walk();
            let string = args.children(&mut cursor).find(|c| c.kind() == "string");
            string?
        }
        _ => return None,
    };
    let text = string_node.utf8_text(src).ok()?;
    Some(text.trim_matches(['"', '\'', '`']).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(lang: Language, src: &str) -> Vec<String> {
        qualified_names(lang, src)
    }

    #[test]
    fn python_collects_imports_and_call_chains() {
        let got = names(
            Language::Python,
            "import requests\nfrom urllib.request import urlopen\n\ndef f(u):\n    return requests.get(u).text\n",
        );
        assert!(got.contains(&"requests".to_string()));
        assert!(got.contains(&"urllib.request".to_string()));
        assert!(got.contains(&"requests.get".to_string()));
    }

    #[test]
    fn python_ignores_comments_and_strings() {
        // The whole point of parsing rather than scanning: a name in prose or in
        // a literal is not an effect acquisition.
        let got = names(
            Language::Python,
            "# we used to call requests.get here\nDOC = 'see requests.post'\nx = 1\n",
        );
        assert!(!got.iter().any(|n| n.starts_with("requests")));
    }

    #[test]
    fn rust_collects_scoped_paths() {
        let got = names(
            Language::Rust,
            "use std::fs;\nfn f() -> String {\n    // std::net::TcpStream is not used\n    std::fs::read_to_string(\"p\").unwrap()\n}\n",
        );
        assert!(got.contains(&"std.fs".to_string()));
        assert!(got.contains(&"std.fs.read_to_string".to_string()));
        assert!(!got.contains(&"std.net.TcpStream".to_string()));
    }

    #[test]
    fn js_collects_member_chains_and_module_specifiers() {
        let got = names(
            Language::JavaScript,
            "import axios from 'axios';\nconst fs = require('fs');\nexport const f = (u) => axios.get(u);\nconst k = process.env.KEY;\n",
        );
        assert!(got.contains(&"axios".to_string()));
        assert!(got.contains(&"fs".to_string()));
        assert!(got.contains(&"axios.get".to_string()));
        assert!(got.contains(&"process.env.KEY".to_string()));
    }

    #[test]
    fn unparseable_source_is_silent() {
        assert!(names(Language::Python, "def f(:\n  ???").is_empty());
    }
}
