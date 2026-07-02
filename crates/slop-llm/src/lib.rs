//! Claude API judge for Tier-3 semantic-redundancy candidates.
//!
//! Raw HTTP against `POST /v1/messages` (no official Rust SDK). One batched
//! request judges all candidate pairs; `output_config.format` with a JSON
//! schema guarantees a parseable verdict list. Verdicts are advisory input —
//! the caller maps them to Advisory findings, never Blocking (D4).

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::json;

pub const DEFAULT_MODEL: &str = "claude-opus-4-8";
const API_URL: &str = "https://api.anthropic.com/v1/messages";

/// One pair for the judge. `context` is the rendered signature/docstring/body
/// text for each side.
pub struct JudgeInput {
    pub index: usize,
    pub a_label: String,
    pub a_context: String,
    pub b_label: String,
    pub b_context: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Verdict {
    pub index: usize,
    pub redundant: bool,
    pub confidence: String, // "high" | "medium" | "low"
    pub reason: String,
}

/// Abstraction so tests can run without the network.
pub trait Judge {
    fn judge(&self, pairs: &[JudgeInput]) -> Result<Vec<Verdict>>;
}

pub struct ClaudeJudge {
    api_key: String,
    pub model: String,
}

impl ClaudeJudge {
    /// Reads `ANTHROPIC_API_KEY` from the environment.
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .context("ANTHROPIC_API_KEY is not set — required for --tier3")?;
        Ok(Self {
            api_key,
            model: DEFAULT_MODEL.to_string(),
        })
    }
}

fn render_prompt(pairs: &[JudgeInput]) -> String {
    let mut prompt = String::from(
        "You are reviewing a Python codebase for semantic redundancy: pairs of functions that \
         serve the same purpose and should be unified, even if implemented differently. \
         Sharing a helper or theme is NOT redundancy — only judge a pair redundant if one \
         function could replace the other (possibly with a small parameter change) and the \
         codebase would lose nothing.\n\nJudge each pair below.\n",
    );
    for pair in pairs {
        prompt.push_str(&format!(
            "\n<pair index=\"{}\">\n<function label=\"{}\">\n{}\n</function>\n<function label=\"{}\">\n{}\n</function>\n</pair>\n",
            pair.index, pair.a_label, pair.a_context, pair.b_label, pair.b_context,
        ));
    }
    prompt
}

impl Judge for ClaudeJudge {
    fn judge(&self, pairs: &[JudgeInput]) -> Result<Vec<Verdict>> {
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        let schema = json!({
            "type": "object",
            "properties": {
                "verdicts": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "index": {"type": "integer"},
                            "redundant": {"type": "boolean"},
                            "confidence": {"type": "string", "enum": ["high", "medium", "low"]},
                            "reason": {"type": "string"}
                        },
                        "required": ["index", "redundant", "confidence", "reason"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["verdicts"],
            "additionalProperties": false
        });

        let body = json!({
            "model": self.model,
            "max_tokens": 4096,
            "output_config": {"format": {"type": "json_schema", "schema": schema}},
            "messages": [{"role": "user", "content": render_prompt(pairs)}],
        });

        let response = ureq::post(API_URL)
            .set("x-api-key", &self.api_key)
            .set("anthropic-version", "2023-06-01")
            .set("content-type", "application/json")
            .send_json(body)
            .map_err(|e| match e {
                ureq::Error::Status(code, resp) => {
                    let text = resp.into_string().unwrap_or_default();
                    anyhow::anyhow!("Claude API returned {code}: {text}")
                }
                other => anyhow::anyhow!("Claude API request failed: {other}"),
            })?;

        #[derive(Deserialize)]
        struct ContentBlock {
            #[serde(rename = "type")]
            kind: String,
            #[serde(default)]
            text: String,
        }
        #[derive(Deserialize)]
        struct Message {
            content: Vec<ContentBlock>,
            stop_reason: Option<String>,
        }
        #[derive(Deserialize)]
        struct VerdictList {
            verdicts: Vec<Verdict>,
        }

        let message: Message = response.into_json().context("parsing API response")?;
        if message.stop_reason.as_deref() == Some("refusal") {
            bail!("Claude declined the request (stop_reason: refusal)");
        }
        let text = message
            .content
            .iter()
            .find(|b| b.kind == "text")
            .map(|b| b.text.as_str())
            .context("no text block in API response")?;
        let list: VerdictList =
            serde_json::from_str(text).context("parsing structured verdicts")?;
        Ok(list.verdicts)
    }
}
