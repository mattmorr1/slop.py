//! Minimal blocking HTTP/1.1 for the reverse proxy. Enough to parse the
//! Anthropic SDK's requests (fixed `Content-Length`, never chunked) and relay
//! the upstream response — including SSE streams — straight back to the
//! client. Deliberately synchronous and thread-per-connection: no tokio,
//! matching the rest of the workspace.

use std::io::{self, BufRead};

fn read_line_bounded<R: BufRead>(
    reader: &mut R,
    line: &mut String,
    limit: usize,
) -> io::Result<usize> {
    line.clear();
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(line.len());
        }
        let take = buffer
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(buffer.len(), |index| index + 1);
        if line.len().saturating_add(take) > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HTTP line exceeds configured limit",
            ));
        }
        let chunk = std::str::from_utf8(&buffer[..take]).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "HTTP headers are not UTF-8")
        })?;
        let ended = chunk.ends_with('\n');
        line.push_str(chunk);
        reader.consume(take);
        if ended {
            return Ok(line.len());
        }
    }
}

/// A parsed client request. `path` includes the query string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Parse one request from `reader`. Returns `Ok(None)` on a clean EOF before
/// any request line (connection closed with nothing to serve).
pub fn parse_request<R: BufRead>(reader: &mut R) -> io::Result<Option<HttpRequest>> {
    parse_request_bounded(reader, 16 * 1024 * 1024)
}

pub fn parse_request_bounded<R: BufRead>(
    reader: &mut R,
    max_body_bytes: usize,
) -> io::Result<Option<HttpRequest>> {
    const MAX_HEADER_BYTES: usize = 64 * 1024;
    let mut request_line = String::new();
    if read_line_bounded(reader, &mut request_line, 8 * 1024)? == 0 {
        return Ok(None);
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    if method.is_empty() || path.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "malformed request line",
        ));
    }

    let mut headers = Vec::new();
    let mut content_length = 0usize;
    let mut header_bytes = request_line.len();
    loop {
        let mut line = String::new();
        if read_line_bounded(
            reader,
            &mut line,
            MAX_HEADER_BYTES.saturating_sub(header_bytes),
        )? == 0
        {
            break;
        }
        let line = line.trim_end_matches(['\r', '\n']);
        header_bytes = header_bytes.saturating_add(line.len());
        if header_bytes > MAX_HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request headers exceed 64 KiB",
            ));
        }
        if line.is_empty() {
            break; // end of headers
        }
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_string();
            let v = v.trim().to_string();
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.parse().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid Content-Length")
                })?;
                if content_length > max_body_bytes {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "request body exceeds configured limit",
                    ));
                }
            }
            headers.push((k, v));
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    Ok(Some(HttpRequest {
        method,
        path,
        headers,
        body,
    }))
}

/// Request headers that must NOT be forwarded upstream: hop-by-hop headers and
/// ones the HTTP client (ureq) sets itself from the body/URL.
pub fn skip_request_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "host" | "content-length" | "connection" | "transfer-encoding" | "accept-encoding"
    )
}

/// Response headers that must NOT be relayed downstream: hop-by-hop headers,
/// and framing/encoding headers that no longer match after the client library
/// has decoded the body (we re-frame with `Connection: close`).
pub fn skip_response_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "transfer-encoding" | "content-length" | "connection" | "content-encoding"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    #[test]
    fn parses_post_with_body() {
        let raw = "POST /v1/messages?beta=true HTTP/1.1\r\n\
                   Host: api.anthropic.com\r\n\
                   Content-Type: application/json\r\n\
                   Content-Length: 13\r\n\
                   \r\n\
                   {\"model\":\"x\"}";
        let mut r = BufReader::new(raw.as_bytes());
        let req = parse_request(&mut r).unwrap().unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/v1/messages?beta=true");
        assert_eq!(req.header("content-type"), Some("application/json"));
        assert_eq!(req.body, b"{\"model\":\"x\"}");
    }

    #[test]
    fn clean_eof_returns_none() {
        let mut r = BufReader::new(&b""[..]);
        assert_eq!(parse_request(&mut r).unwrap(), None);
    }

    #[test]
    fn skip_lists_are_case_insensitive() {
        assert!(skip_request_header("Content-Length"));
        assert!(skip_request_header("HOST"));
        assert!(!skip_request_header("x-api-key"));
        assert!(skip_response_header("Content-Encoding"));
        assert!(!skip_response_header("content-type"));
    }
}
