//! Token-usage extraction from an Anthropic `/v1/messages` response body,
//! for the proxy's observability log. Handles both the non-streaming JSON
//! shape (`{model, usage:{input_tokens, output_tokens}}`) and the streamed
//! SSE shape (`message_start` carries input tokens + model, `message_delta`
//! carries the final cumulative output tokens).

use serde_json::Value;

/// Keeps the beginning and a rolling tail of a streamed response. Anthropic's
/// input usage is in `message_start` and final output usage is near the end, so
/// this preserves both without retaining an unbounded generation in memory.
pub struct BoundedCapture {
    head: Vec<u8>,
    tail: Vec<u8>,
    head_limit: usize,
    tail_limit: usize,
    cursor: usize,
    wrapped: bool,
}

impl BoundedCapture {
    pub fn new(limit: usize) -> Self {
        let head_limit = if limit < 2 {
            limit
        } else {
            (limit / 2).min(64 * 1024)
        };
        Self {
            head: Vec::with_capacity(head_limit),
            tail: Vec::with_capacity(limit.saturating_sub(head_limit)),
            head_limit,
            tail_limit: limit.saturating_sub(head_limit),
            cursor: 0,
            wrapped: false,
        }
    }

    pub fn extend(&mut self, bytes: &[u8]) {
        let head_take = (self.head_limit - self.head.len()).min(bytes.len());
        self.head.extend_from_slice(&bytes[..head_take]);
        if self.tail_limit == 0 {
            return;
        }
        for &byte in &bytes[head_take..] {
            if self.tail.len() < self.tail_limit {
                self.tail.push(byte);
            } else {
                self.tail[self.cursor] = byte;
                self.cursor = (self.cursor + 1) % self.tail_limit;
                self.wrapped = true;
            }
        }
    }

    pub fn finish(mut self) -> Vec<u8> {
        if self.wrapped {
            self.tail.rotate_left(self.cursor);
        }
        if !self.head.is_empty() && !self.tail.is_empty() {
            self.head.push(b'\n');
        }
        self.head.extend_from_slice(&self.tail);
        self.head
    }
}

#[derive(Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Usage {
    pub model: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

fn take_u64(v: &Value, path: &[&str]) -> Option<u64> {
    let mut cur = v;
    for p in path {
        cur = cur.get(p)?;
    }
    cur.as_u64()
}

fn take_str(v: &Value, path: &[&str]) -> Option<String> {
    let mut cur = v;
    for p in path {
        cur = cur.get(p)?;
    }
    cur.as_str().map(str::to_string)
}

/// Fold one JSON event into the running totals. `usage` fields can live at the
/// top level (non-streaming) or under `message` (SSE `message_start`).
fn merge(usage: &mut Usage, v: &Value) {
    if usage.model.is_none() {
        usage.model = take_str(v, &["model"]).or_else(|| take_str(v, &["message", "model"]));
    }
    if let Some(input) = take_u64(v, &["usage", "input_tokens"])
        .or_else(|| take_u64(v, &["message", "usage", "input_tokens"]))
    {
        // input tokens are fixed for the request; keep the first seen.
        usage.input_tokens.get_or_insert(input);
    }
    if let Some(output) = take_u64(v, &["usage", "output_tokens"])
        .or_else(|| take_u64(v, &["message", "usage", "output_tokens"]))
    {
        // output tokens grow across deltas; keep the largest (the final).
        usage.output_tokens = Some(usage.output_tokens.map_or(output, |o| o.max(output)));
    }
}

/// Best-effort usage extraction. Never fails — an unparseable body just
/// yields an empty `Usage`.
pub fn extract(body: &[u8]) -> Usage {
    let text = String::from_utf8_lossy(body);
    let mut usage = Usage::default();
    let mut saw_sse = false;

    for line in text.lines() {
        let Some(data) = line.trim_start().strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(data) {
            saw_sse = true;
            merge(&mut usage, &v);
        }
    }

    if !saw_sse {
        if let Ok(v) = serde_json::from_str::<Value>(text.trim()) {
            merge(&mut usage, &v);
        }
    }
    usage
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_from_sse_stream() {
        let body = "\
event: message_start
data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-x\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}

event: message_delta
data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":25}}

data: [DONE]
";
        let u = extract(body.as_bytes());
        assert_eq!(u.model.as_deref(), Some("claude-x"));
        assert_eq!(u.input_tokens, Some(10));
        assert_eq!(u.output_tokens, Some(25));
    }

    #[test]
    fn extracts_from_plain_json() {
        let body = r#"{"model":"claude-y","usage":{"input_tokens":7,"output_tokens":3}}"#;
        let u = extract(body.as_bytes());
        assert_eq!(u.model.as_deref(), Some("claude-y"));
        assert_eq!(u.input_tokens, Some(7));
        assert_eq!(u.output_tokens, Some(3));
    }

    #[test]
    fn unparseable_body_is_empty_not_an_error() {
        assert_eq!(extract(b"not json at all"), Usage::default());
    }

    #[test]
    fn bounded_capture_keeps_head_and_tail() {
        let mut capture = BoundedCapture::new(10);
        capture.extend(b"abcdefghijklmnop");
        let out = capture.finish();
        assert!(out.starts_with(b"abcde"));
        assert!(out.ends_with(b"lmnop"));
        assert!(out.len() <= 11);
    }
}
