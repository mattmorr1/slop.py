//! Claude API judge for Tier-3 semantic-redundancy candidates.
//!
//! Raw HTTP against `POST /v1/messages` (no official Rust SDK). One batched
//! request judges all candidate pairs; `output_config.format` with a JSON
//! schema guarantees a parseable verdict list. Verdicts are advisory input —
//! the caller maps them to Advisory findings, never Blocking (D4).

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

pub mod ollama;

pub const DEFAULT_MODEL: &str = "claude-opus-4-8";
const API_URL: &str = "https://api.anthropic.com/v1/messages";

/// Select a judge backend from the environment: `SLOP_JUDGE=ollama` uses the
/// local Ollama model (free, no key), anything else uses Claude. Lets Tier-3
/// run offline.
pub fn judge_from_env() -> Result<Box<dyn Judge>> {
    match std::env::var("SLOP_JUDGE").as_deref() {
        Ok("ollama") => Ok(Box::new(ollama::Ollama::from_env())),
        _ => Ok(Box::new(ClaudeJudge::from_env()?)),
    }
}

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

/// Abstraction so tests can run without the network and backends (Claude /
/// Ollama) are interchangeable. Backends implement only the transport
/// (`judge_prompt` + `model`); the two question shapes are default methods over
/// their own prompt renderers, so a new judged rule is a renderer, not a
/// per-backend request.
pub trait Judge {
    /// Execute one batched structured-verdict request for a rendered prompt.
    fn judge_prompt(&self, prompt: &str) -> Result<Vec<Verdict>>;
    /// Model identifier, for the "judging N via <model>" log line.
    fn model(&self) -> &str;

    /// Semantic-redundancy verdicts for candidate pairs (`redundant` = the two
    /// serve the same purpose).
    fn judge(&self, pairs: &[JudgeInput]) -> Result<Vec<Verdict>> {
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        self.judge_prompt(&render_prompt(pairs))
    }

    /// Trivial-wrapper verdicts (`redundant` = the wrapper's name adds nothing
    /// the call site would miss — inline it).
    fn judge_wrappers(&self, wrappers: &[JudgeInput]) -> Result<Vec<Verdict>> {
        if wrappers.is_empty() {
            return Ok(Vec::new());
        }
        self.judge_prompt(&render_wrapper_prompt(wrappers))
    }
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

/// JSON Schema for a `{verdicts: [...]}` payload — shared by the Claude
/// (`output_config`) and Ollama (`format`) structured-output paths.
pub(crate) fn verdict_schema() -> Value {
    json!({
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
    })
}

pub(crate) fn render_prompt(pairs: &[JudgeInput]) -> String {
    let mut prompt = String::from(
        "You are reviewing a codebase for semantic redundancy: pairs of functions that \
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

/// Prompt for the trivial-wrapper gate: for each one-line forwarder, does its
/// name carry meaning the call site would lose? `a_label` is the wrapper name,
/// `b_label` the callee it delegates to, `a_context` its signature/caller
/// context. `redundant=true` means the name is slop and the wrapper should be
/// inlined; `false` means the name states intent the callee doesn't.
pub(crate) fn render_wrapper_prompt(wrappers: &[JudgeInput]) -> String {
    let mut prompt = String::from(
        "You are reviewing a codebase for trivial wrapper functions: one-line functions \
         whose entire body forwards their arguments to a single other call. Some are slop and \
         should be inlined at their few call sites; others earn their place because the NAME \
         expresses a domain concept, a policy, or an abstraction boundary the raw call would \
         lose.\n\nFor each wrapper below decide: if you replaced every call to it with the \
         delegated call, would the code lose meaning the name was carrying? If the name adds \
         nothing beyond the callee (a mechanical rename or a redundant synonym), answer \
         redundant=true (inline it). If the name states intent the callee does not, answer \
         redundant=false (keep it).\n",
    );
    for w in wrappers {
        prompt.push_str(&format!(
            "\n<wrapper index=\"{}\" name=\"{}\" delegates_to=\"{}\">\n{}\n</wrapper>\n",
            w.index, w.a_label, w.b_label, w.a_context,
        ));
    }
    prompt
}

impl Judge for ClaudeJudge {
    fn model(&self) -> &str {
        &self.model
    }

    fn judge_prompt(&self, prompt: &str) -> Result<Vec<Verdict>> {
        let schema = verdict_schema();

        let body = json!({
            "model": self.model,
            "max_tokens": 4096,
            "output_config": {"format": {"type": "json_schema", "schema": schema}},
            "messages": [{"role": "user", "content": prompt}],
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Spy(RefCell<String>);
    impl Judge for Spy {
        fn model(&self) -> &str {
            "spy"
        }
        fn judge_prompt(&self, prompt: &str) -> Result<Vec<Verdict>> {
            *self.0.borrow_mut() = prompt.to_string();
            Ok(Vec::new())
        }
    }

    fn input() -> JudgeInput {
        JudgeInput {
            index: 0,
            a_label: "load".into(),
            a_context: "def load(path):".into(),
            b_label: "read_file".into(),
            b_context: String::new(),
        }
    }

    #[test]
    fn wrapper_and_redundancy_route_distinct_prompts() {
        let spy = Spy::default();
        spy.judge_wrappers(&[input()]).unwrap();
        let wrapper = spy.0.borrow().clone();
        spy.judge(&[input()]).unwrap();
        let redundancy = spy.0.borrow().clone();
        assert!(wrapper.contains("trivial wrapper") && wrapper.contains("delegates_to=\"read_file\""));
        assert!(redundancy.contains("semantic redundancy"));
        assert_ne!(wrapper, redundancy);
    }

    #[test]
    fn empty_inputs_skip_the_transport() {
        let spy = Spy::default();
        assert!(spy.judge_wrappers(&[]).unwrap().is_empty());
        assert!(spy.judge(&[]).unwrap().is_empty());
        assert!(spy.0.borrow().is_empty(), "transport must not run for empty input");
    }
}
