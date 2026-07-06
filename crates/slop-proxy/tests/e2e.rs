//! End-to-end proxy test, network-free: a local mock stands in for the
//! Anthropic API. A client connects through the proxy, and we assert the SSE
//! body is relayed byte-for-byte and the usage log records the token counts.

use std::io::{BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use slop_proxy::http::parse_request;
use slop_proxy::{serve_on, ProxyConfig};

const SSE_BODY: &str = "\
event: message_start
data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-x\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}

event: message_delta
data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":25}}

data: [DONE]
";

/// A one-shot mock upstream: reads the forwarded request, replies with a
/// fixed SSE response, closes. Returns its address.
fn spawn_mock_upstream() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            // Consume the request (incl. body) so the socket is drained.
            let _ = parse_request(&mut reader);
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n",
                SSE_BODY.len()
            );
            let _ = stream.write_all(SSE_BODY.as_bytes());
            let _ = stream.flush();
        }
    });
    addr
}

fn unique_log_path() -> PathBuf {
    std::env::temp_dir().join(format!("slop-proxy-test-{}.jsonl", std::process::id()))
}

#[test]
fn relays_sse_and_logs_usage() {
    let mock_addr = spawn_mock_upstream();
    let log = unique_log_path();
    let _ = std::fs::remove_file(&log);

    let proxy_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    let upstream = format!("http://{mock_addr}");
    let log_for_proxy = log.clone();
    std::thread::spawn(move || {
        serve_on(
            proxy_listener,
            ProxyConfig {
                port: 0, // unused: listener already bound
                upstream,
                repo: None,
                steer: false,
                log: Some(log_for_proxy),
            },
        )
        .unwrap();
    });

    // Client request through the proxy.
    let mut client = TcpStream::connect(proxy_addr).unwrap();
    let body = r#"{"model":"claude-x","messages":[]}"#;
    write!(
        client,
        "POST /v1/messages HTTP/1.1\r\nHost: x\r\nx-api-key: test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .unwrap();
    client.flush().unwrap();

    let mut response = String::new();
    client.read_to_string(&mut response).unwrap();

    assert!(response.starts_with("HTTP/1.1 200"), "status line: {response}");
    assert!(response.contains("message_start"), "body relayed: {response}");
    assert!(response.contains("claude-x"));
    // The SSE payload is passed through verbatim.
    assert!(response.contains(SSE_BODY.trim_end()));

    // Usage log is written before the connection closes; poll briefly to be safe.
    let deadline = Instant::now() + Duration::from_secs(2);
    let contents = loop {
        if let Ok(c) = std::fs::read_to_string(&log) {
            if !c.trim().is_empty() {
                break c;
            }
        }
        if Instant::now() > deadline {
            panic!("usage log never written to {}", log.display());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let record: serde_json::Value = serde_json::from_str(contents.lines().next().unwrap()).unwrap();
    assert_eq!(record["status"], 200);
    assert_eq!(record["model"], "claude-x");
    assert_eq!(record["input_tokens"], 10);
    assert_eq!(record["output_tokens"], 25);
    assert_eq!(record["path"], "/v1/messages");

    let _ = std::fs::remove_file(&log);
}
