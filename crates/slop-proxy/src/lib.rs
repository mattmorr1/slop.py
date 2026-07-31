//! `ANTHROPIC_BASE_URL` reverse proxy (M5, D10): agent-agnostic steering and
//! token observability, without cert-MITM. Point any Anthropic client at it
//! (`ANTHROPIC_BASE_URL=http://localhost:8787`); slop relays every request to
//! the real API, streams the response back untouched (SSE included), logs
//! token usage, and — with `--steer` — augments the request's system prompt
//! with the repo's sanctioned-channel policy.
//!
//! Synchronous, thread-per-connection (`std::net`); upstream calls go through
//! `ureq`, exactly like `slop-llm`. No tokio.

use std::io::{BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::{json, Value};

pub mod http;
pub mod usage;

pub struct ProxyConfig {
    pub port: u16,
    /// Upstream base URL, e.g. `https://api.anthropic.com`.
    pub upstream: String,
    /// Repo whose sanctioned-channel policy is injected when `steer` is set.
    pub repo: Option<PathBuf>,
    pub steer: bool,
    /// JSONL observability log path.
    pub log: Option<PathBuf>,
}

/// Immutable per-run context shared across connection threads.
struct Ctx {
    upstream: String,
    log: Option<PathBuf>,
    /// Precomputed steering text (None unless `--steer` + a repo with a policy).
    steering: Option<String>,
}

/// Bind to the configured port on localhost and serve until the process exits.
pub fn serve(config: ProxyConfig) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", config.port))
        .with_context(|| format!("binding 127.0.0.1:{}", config.port))?;
    eprintln!(
        "slop proxy: 127.0.0.1:{} -> {} (steer={}, log={})",
        config.port,
        config.upstream,
        config.steer,
        config.log.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "off".into()),
    );
    serve_on(listener, config)
}

/// The same world model the `SessionStart` hook injects — sanctioned channels,
/// layer rules, and the capability index — rendered once at startup.
///
/// The hook only reaches Claude Code; this is the surface every other
/// Anthropic client gets, and it used to carry the channel list alone, which
/// says what to route through but not what already exists. Building the graph
/// costs one pass at launch rather than one per request, and degrades to
/// policy-only when the repo has no index (D8: silent when there's nothing to
/// say). Compression stays on the hook path — the proxy is too late for it
/// (D10).
fn world_model(repo: &std::path::Path) -> Option<String> {
    let policy = slop_analyze::policy::Policy::load(repo).ok()?;
    let analysis = slop_analyze::check::load_analysis(repo, None).ok();
    let env = analysis
        .as_ref()
        .map(|a| slop_analyze::config::env_vars(repo, &a.facts))
        .unwrap_or_default();
    slop_analyze::world::render(
        &policy,
        analysis.as_ref().map(|a| &a.built),
        &env,
        slop_analyze::world::DEFAULT_BUDGET,
    )
}

/// Serve on an already-bound listener. Split out so tests can bind an
/// ephemeral port and drive the proxy directly.
pub fn serve_on(listener: TcpListener, config: ProxyConfig) -> Result<()> {
    let steering = if config.steer {
        config.repo.as_deref().and_then(world_model)
    } else {
        None
    };
    let ctx = Arc::new(Ctx {
        upstream: config.upstream.trim_end_matches('/').to_string(),
        log: config.log,
        steering,
    });

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("slop proxy: accept error: {e}");
                continue;
            }
        };
        let ctx = Arc::clone(&ctx);
        std::thread::spawn(move || {
            if let Err(e) = handle_connection(stream, &ctx) {
                eprintln!("slop proxy: connection error: {e}");
            }
        });
    }
    Ok(())
}

fn handle_connection(stream: TcpStream, ctx: &Ctx) -> Result<()> {
    let mut writer = stream.try_clone().context("cloning stream")?;
    let mut reader = BufReader::new(stream);

    let Some(req) = http::parse_request(&mut reader)? else {
        return Ok(()); // client closed with nothing to serve
    };

    // Steering: augment the system prompt on message-creation requests.
    let body = if ctx.steering.is_some() && req.path.contains("/messages") && req.method == "POST" {
        inject_steering(&req.body, ctx.steering.as_deref().unwrap())
    } else {
        req.body.clone()
    };

    // Forward upstream via ureq, preserving auth/version headers.
    let url = format!("{}{}", ctx.upstream, req.path);
    let mut up = ureq::request(&req.method, &url);
    for (k, v) in &req.headers {
        if !http::skip_request_header(k) {
            up = up.set(k, v);
        }
    }
    let resp = match up.send_bytes(&body) {
        Ok(r) => r,
        // Non-2xx still carries a response to relay (e.g. a 400 from the API).
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => {
            let msg = format!("slop proxy: upstream error: {e}");
            let _ = write!(
                writer,
                "HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                msg.len(),
                msg
            );
            return Ok(());
        }
    };

    // Relay status + headers, re-framed with Connection: close so the body can
    // stream to EOF (works for SSE without chunked reassembly).
    let status = resp.status();
    write!(writer, "HTTP/1.1 {} {}\r\n", status, resp.status_text())?;
    for name in resp.headers_names() {
        if http::skip_response_header(&name) {
            continue;
        }
        if let Some(val) = resp.header(&name) {
            write!(writer, "{name}: {val}\r\n")?;
        }
    }
    write!(writer, "Connection: close\r\n\r\n")?;
    writer.flush()?;

    // Stream the body through, teeing a copy for usage extraction. Flush per
    // chunk so SSE tokens reach the client as they arrive.
    let mut body_reader = resp.into_reader();
    let mut captured = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = body_reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n])?;
        writer.flush()?;
        captured.extend_from_slice(&buf[..n]);
    }

    log_usage(ctx, &req.path, status, &captured);
    Ok(())
}

/// Prepend steering to the request's `system` prompt. Handles the string form
/// and the content-block-array form; adds one if absent. Returns the original
/// bytes unchanged if the body isn't the JSON object we expect.
fn inject_steering(body: &[u8], steering: &str) -> Vec<u8> {
    let Ok(mut v) = serde_json::from_slice::<Value>(body) else {
        return body.to_vec();
    };
    let Some(obj) = v.as_object_mut() else {
        return body.to_vec();
    };
    match obj.get_mut("system") {
        Some(Value::String(s)) => {
            *s = format!("{steering}\n\n{s}");
        }
        Some(Value::Array(blocks)) => {
            blocks.insert(0, json!({ "type": "text", "text": steering }));
        }
        _ => {
            obj.insert("system".to_string(), Value::String(steering.to_string()));
        }
    }
    serde_json::to_vec(&v).unwrap_or_else(|_| body.to_vec())
}

fn log_usage(ctx: &Ctx, path: &str, status: u16, captured: &[u8]) {
    let Some(log_path) = &ctx.log else {
        return;
    };
    let usage = usage::extract(captured);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let record = json!({
        "ts": ts,
        "path": path,
        "status": status,
        "model": usage.model,
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
    });
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
    {
        let _ = writeln!(f, "{record}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject_steering_into_string_system() {
        let body = br#"{"model":"x","system":"You are helpful."}"#;
        let out = inject_steering(body, "STEER");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["system"], "STEER\n\nYou are helpful.");
    }

    #[test]
    fn inject_steering_into_array_system() {
        let body = br#"{"system":[{"type":"text","text":"base"}]}"#;
        let out = inject_steering(body, "STEER");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["system"][0]["text"], "STEER");
        assert_eq!(v["system"][1]["text"], "base");
    }

    #[test]
    fn inject_steering_adds_absent_system() {
        let out = inject_steering(br#"{"model":"x"}"#, "STEER");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["system"], "STEER");
    }

    #[test]
    fn inject_steering_leaves_non_json_untouched() {
        assert_eq!(inject_steering(b"not json", "STEER"), b"not json");
    }
}
