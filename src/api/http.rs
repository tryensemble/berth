//! Just enough HTTP/1.1 for the daemon's socket: one request per connection,
//! JSON bodies, and `Upgrade` for streams.

use std::io::{self, Read, Write};

use anyhow::bail;

const MAX_HEAD: usize = 64 << 10;

pub struct Request {
    pub method: String,
    pub path: String,
    pub upgrade: bool,
    pub body: Vec<u8>,
}

/// Reads up to the blank line after the headers, one byte at a time, so
/// nothing that follows an upgrade is swallowed by a buffer.
pub fn read_head(s: &mut impl Read) -> anyhow::Result<String> {
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if s.read(&mut b)? == 0 {
            bail!("connection closed");
        }
        head.push(b[0]);
        if head.len() > MAX_HEAD {
            bail!("headers too large");
        }
    }
    Ok(String::from_utf8_lossy(&head).into_owned())
}

/// The value of a header, by case-insensitive name.
pub fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

fn content_length(head: &str) -> usize {
    header(head, "content-length").and_then(|v| v.parse().ok()).unwrap_or(0)
}

pub fn read_request(s: &mut impl Read) -> anyhow::Result<Request> {
    let head = read_head(s)?;
    let mut first = head.lines().next().unwrap_or("").split_whitespace();
    let method = first.next().unwrap_or("").to_string();
    let path = first.next().unwrap_or("").to_string();
    let upgrade = header(&head, "upgrade").is_some();
    let mut body = vec![0u8; content_length(&head)];
    s.read_exact(&mut body)?;
    Ok(Request { method, path, upgrade, body })
}

pub fn write_request(s: &mut impl Write, method: &str, path: &str, body: &[u8], upgrade: bool) -> io::Result<()> {
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: berth\r\nContent-Length: {}\r\n", body.len());
    if upgrade {
        req.push_str("Connection: Upgrade\r\nUpgrade: berth\r\n");
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes())?;
    s.write_all(body)
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        101 => "Switching Protocols",
        400 => "Bad Request",
        404 => "Not Found",
        502 => "Bad Gateway",
        _ => "Error",
    }
}

pub fn respond(s: &mut impl Write, status: u16, body: &serde_json::Value) -> io::Result<()> {
    let body = serde_json::to_vec(body).unwrap();
    write!(
        s,
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reason(status),
        body.len()
    )?;
    s.write_all(&body)
}

pub fn respond_upgrade(s: &mut impl Write) -> io::Result<()> {
    s.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: berth\r\nConnection: Upgrade\r\n\r\n")
}

pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Reads a response's head and, unless it switched protocols, its body.
pub fn read_response(s: &mut impl Read) -> anyhow::Result<Response> {
    let head = read_head(s)?;
    let status: u16 = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    if status == 101 {
        return Ok(Response { status, body: Vec::new() });
    }
    let mut body = vec![0u8; content_length(&head)];
    s.read_exact(&mut body)?;
    Ok(Response { status, body })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn requests_round_trip() {
        let mut wire = Vec::new();
        write_request(&mut wire, "POST", "/v1/berths", br#"{"a":1}"#, false).unwrap();
        let req = read_request(&mut &wire[..]).unwrap();
        assert_eq!((req.method.as_str(), req.path.as_str(), req.upgrade), ("POST", "/v1/berths", false));
        assert_eq!(req.body, br#"{"a":1}"#);
    }

    #[test]
    fn an_upgrade_leaves_what_follows_unread() {
        let mut wire = Vec::new();
        write_request(&mut wire, "POST", "/v1/berths/x/exec", b"{}", true).unwrap();
        wire.extend_from_slice(b"FRAMES");
        let mut r = &wire[..];
        let req = read_request(&mut r).unwrap();
        assert!(req.upgrade);
        assert_eq!(r, b"FRAMES");
    }

    #[test]
    fn headers_are_case_insensitive() {
        let head = "GET / HTTP/1.1\r\ncontent-LENGTH: 12\r\nX-Other: a:b\r\n\r\n";
        assert_eq!(header(head, "Content-Length"), Some("12"));
        assert_eq!(header(head, "x-other"), Some("a:b"));
        assert_eq!(header(head, "missing"), None);
    }

    #[test]
    fn responses_round_trip() {
        let mut wire = Vec::new();
        respond(&mut wire, 404, &json!({ "error": "no berth x" })).unwrap();
        assert!(wire.starts_with(b"HTTP/1.1 404 Not Found\r\n"));
        let resp = read_response(&mut &wire[..]).unwrap();
        assert_eq!(resp.status, 404);
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&resp.body).unwrap()["error"], "no berth x");

        let mut wire = Vec::new();
        respond_upgrade(&mut wire).unwrap();
        wire.extend_from_slice(b"rest");
        let mut r = &wire[..];
        assert_eq!(read_response(&mut r).unwrap().status, 101);
        assert_eq!(r, b"rest");
    }

    #[test]
    fn a_closed_connection_or_huge_head_fails() {
        assert!(read_head(&mut &b"GET / HTTP/1.1\r\n"[..]).is_err());
        let huge = vec![b'a'; MAX_HEAD + 10];
        assert!(read_head(&mut &huge[..]).is_err());
    }
}
