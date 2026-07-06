//! The pure core of the LSP surface: run slop's whole-repo check and shape the
//! findings into LSP `Diagnostic` objects, keyed by the `file://` URI they
//! belong to. No I/O framing here — that lives in `server` — so this is what
//! the headless tests exercise directly.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use serde_json::{json, Value};
use slop_analyze::check::{self, CheckRequest};
use slop_analyze::findings::{Finding, Severity};

/// Run the whole-repo check for `repo` and group the findings into LSP
/// diagnostics keyed by absolute `file://` URI. Whole-repo (`all: true`)
/// rather than diff-relative: an editor wants every finding touching the files
/// it has open, not just what changed since HEAD.
pub fn compute(repo: &Path, index: Option<&Path>) -> Result<BTreeMap<String, Vec<Value>>> {
    let result = check::run(CheckRequest {
        repo: repo.to_path_buf(),
        index: index.map(Path::to_path_buf),
        policy: None,
        all: true,
        tier3: false,
        base: "HEAD".to_string(),
    })?;

    let mut by_uri: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for f in &result.findings {
        let uri = path_to_file_uri(&repo.join(&f.file));
        by_uri.entry(uri).or_default().push(to_diagnostic(f));
    }
    Ok(by_uri)
}

/// Map a slop severity onto the LSP `DiagnosticSeverity` scale:
/// 1 = Error, 2 = Warning, 3 = Information, 4 = Hint.
fn lsp_severity(sev: Severity) -> u8 {
    match sev {
        Severity::Blocking => 1,
        Severity::Warning => 2,
        Severity::Advisory => 3,
    }
}

/// One slop finding as an LSP `Diagnostic`. The range covers the offending
/// entity's body: findings carry 0-based lines (already LSP's convention). A
/// module-level finding can carry `usize::MAX` as its end line (no real body
/// span) — clamp that to a single full-line highlight so we never emit an
/// absurd range.
fn to_diagnostic(f: &Finding) -> Value {
    let (start_line, raw_end) = f.lines;
    // Treat a missing/absurd end (module-level findings, or end < start) as a
    // single-line highlight; cap huge but plausible spans defensively.
    let end_line = if raw_end < start_line || raw_end.saturating_sub(start_line) > 5000 {
        start_line
    } else {
        raw_end
    };
    // LSP highlights a whole line as [line, 0)..[line + 1, 0).
    json!({
        "range": {
            "start": { "line": start_line, "character": 0 },
            "end": { "line": end_line + 1, "character": 0 },
        },
        "severity": lsp_severity(f.severity),
        "source": "slop",
        "code": f.rule,
        "message": format!("{}\nfix: {}", f.message, f.fix_guidance),
    })
}

/// Absolute filesystem path to a `file://` URI, percent-encoding everything
/// outside the RFC 3986 unreserved set except the path separator. Enough for
/// real-world paths (spaces, unicode) without pulling in a url crate.
pub fn path_to_file_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for &b in path.to_string_lossy().as_bytes() {
        match b {
            b'/' | b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Inverse of [`path_to_file_uri`]: a `file://` URI back to a filesystem path,
/// percent-decoding the body. Returns the input unchanged if it isn't a
/// `file://` URI. Used to track which document a `didOpen`/`didSave` refers to.
pub fn file_uri_to_path(uri: &str) -> String {
    let Some(rest) = uri.strip_prefix("file://") else {
        return uri.to_string();
    };
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&rest[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_roundtrips_paths_with_spaces() {
        let uri = path_to_file_uri(Path::new("/tmp/my repo/a.py"));
        assert_eq!(uri, "file:///tmp/my%20repo/a.py");
        assert_eq!(file_uri_to_path(&uri), "/tmp/my repo/a.py");
    }

    #[test]
    fn module_level_max_end_line_clamps_to_single_line() {
        let f = Finding {
            rule: "circular-import",
            severity: Severity::Blocking,
            entity: "services.notify".into(),
            file: "services/notify.py".into(),
            lines: (0, usize::MAX),
            message: "cycle".into(),
            fix_guidance: "break it".into(),
        };
        let d = to_diagnostic(&f);
        assert_eq!(d["range"]["start"]["line"], 0);
        assert_eq!(d["range"]["end"]["line"], 1);
        assert_eq!(d["severity"], 1);
    }
}
