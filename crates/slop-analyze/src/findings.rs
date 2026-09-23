//! Findings + severity (D12). Confidence is intrinsic to the detector:
//! deterministic detectors may block, fuzzy detectors are structurally
//! incapable of it.

use serde::ser::{SerializeStruct, Serializer};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct FindingId(String);

impl std::fmt::Display for FindingId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for FindingId {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            Ok(Self(value.to_ascii_lowercase()))
        } else {
            Err("finding id must be a 64-character hexadecimal digest")
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EvidenceLocus {
    pub entity: String,
    pub file: String,
    pub lines: (usize, usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Severity {
    Advisory,
    Warning,
    Blocking,
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Severity::Blocking => write!(f, "BLOCKING"),
            Severity::Warning => write!(f, "WARNING"),
            Severity::Advisory => write!(f, "ADVISORY"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub rule: &'static str,
    pub severity: Severity,
    /// Entity ID, e.g. `services.alerts::send_alert`.
    pub entity: String,
    pub file: String,
    /// 0-based line range of the offending entity's body.
    pub lines: (usize, usize),
    /// Other entities participating in a relational finding. The primary
    /// entity above remains the stable presentation anchor.
    pub related: Vec<EvidenceLocus>,
    pub message: String,
    /// Machine-readable resolution hint (D9): what a fixer — human or agent —
    /// should do instead.
    pub fix_guidance: String,
}

impl Serialize for Finding {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("Finding", 10)?;
        state.serialize_field("id", &self.id())?;
        state.serialize_field("rule", self.rule)?;
        state.serialize_field("severity", &self.severity)?;
        state.serialize_field("entity", &self.entity)?;
        state.serialize_field("file", &self.file)?;
        state.serialize_field("lines", &self.lines)?;
        if !self.related.is_empty() {
            state.serialize_field("related", &self.related)?;
        }
        state.serialize_field("message", &self.message)?;
        state.serialize_field("fix_guidance", &self.fix_guidance)?;
        state.end()
    }
}

impl Finding {
    pub fn id(&self) -> FindingId {
        let mut hasher = blake3::Hasher::new();
        for value in [self.rule, &self.entity, &self.file] {
            hasher.update(value.as_bytes());
            hasher.update(&[0]);
        }
        hasher.update(&self.lines.0.to_le_bytes());
        hasher.update(&self.lines.1.to_le_bytes());
        FindingId(hasher.finalize().to_hex().to_string())
    }

    pub fn evidence_loci(&self) -> Vec<EvidenceLocus> {
        let mut loci = Vec::with_capacity(self.related.len() + 1);
        loci.push(EvidenceLocus {
            entity: self.entity.clone(),
            file: self.file.clone(),
            lines: self.lines,
        });
        loci.extend(self.related.iter().cloned());
        loci.sort_by(|a, b| {
            a.file
                .cmp(&b.file)
                .then_with(|| a.lines.cmp(&b.lines))
                .then_with(|| a.entity.cmp(&b.entity))
        });
        loci.dedup();
        loci
    }
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "{} [{}] {} ({}:{})",
            self.severity,
            self.rule,
            self.entity,
            self.file,
            self.lines.0 + 1,
        )?;
        writeln!(f, "  {}", self.message)?;
        write!(f, "  fix: {}", self.fix_guidance)
    }
}
