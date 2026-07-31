//! Local LLM backend via Ollama (`http://localhost:11434`). Free and offline
//! — used for Tier-3 judging without an API key and, more importantly, to
//! densify skeletons with a one-line semantic summary during compression.
//! Same synchronous `ureq` style as the Claude backend; no async.

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;

use crate::{verdict_schema, Judge, Verdict};

pub const DEFAULT_MODEL: &str = "qwen2.5:1.5b";
const DEFAULT_HOST: &str = "http://localhost:11434";

pub struct Ollama {
    host: String,
    pub model: String,
}

impl Ollama {
    /// `OLLAMA_HOST` (default `http://localhost:11434`) and `OLLAMA_MODEL`
    /// (default `qwen2.5:1.5b`).
    pub fn from_env() -> Self {
        Self {
            host: std::env::var("OLLAMA_HOST").unwrap_or_else(|_| DEFAULT_HOST.to_string()),
            model: std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
        }
    }

    /// One-shot completion via `POST /api/generate` (non-streaming).
    pub fn generate(&self, prompt: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Resp {
            response: String,
        }
        let body = json!({
            "model": self.model,
            "prompt": prompt,
            "stream": false,
            "options": { "temperature": 0 },
        });
        let resp: Resp = ureq::post(&format!("{}/api/generate", self.host))
            .send_json(body)
            .map_err(ollama_err)?
            .into_json()
            .context("parsing ollama /api/generate response")?;
        Ok(resp.response)
    }

    /// A terse (<= ~10 word) summary of what `code` does — for skeleton
    /// densification. Returns a single trimmed line.
    pub fn summarize(&self, code: &str) -> Result<String> {
        let prompt = format!(
            "In 10 words or fewer, say what this code does. Reply with only the phrase, \
             no punctuation, no preamble.\n\n{code}"
        );
        let raw = self.generate(&prompt)?;
        Ok(raw.lines().next().unwrap_or("").trim().to_string())
    }
}

fn ollama_err(e: ureq::Error) -> anyhow::Error {
    match e {
        ureq::Error::Status(code, resp) => {
            let text = resp.into_string().unwrap_or_default();
            anyhow::anyhow!("ollama returned {code}: {text}")
        }
        other => anyhow::anyhow!(
            "ollama request failed ({other}) — is `ollama serve` running on {DEFAULT_HOST}?"
        ),
    }
}

impl Judge for Ollama {
    fn model(&self) -> &str {
        &self.model
    }

    fn judge_prompt(&self, prompt: &str) -> Result<Vec<Verdict>> {
        // Ollama's structured-output `format` takes a JSON Schema directly.
        let body = json!({
            "model": self.model,
            "messages": [{ "role": "user", "content": prompt }],
            "stream": false,
            "format": verdict_schema(),
            "options": { "temperature": 0 },
        });

        #[derive(Deserialize)]
        struct Msg {
            content: String,
        }
        #[derive(Deserialize)]
        struct Resp {
            message: Msg,
        }
        #[derive(Deserialize)]
        struct VerdictList {
            verdicts: Vec<Verdict>,
        }

        let resp: Resp = ureq::post(&format!("{}/api/chat", self.host))
            .send_json(body)
            .map_err(ollama_err)?
            .into_json()
            .context("parsing ollama /api/chat response")?;
        let list: VerdictList = serde_json::from_str(&resp.message.content)
            .context("parsing structured verdicts from ollama")?;
        Ok(list.verdicts)
    }
}
