//! The wire: a small HTTP/1.1 over the daemon's Unix socket, and the frames an
//! exec stream speaks once the connection is upgraded.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};

pub const SOCKET: &str = "/run/berth/berth.sock";

// Frame types of an exec stream.
pub const STDIN: u8 = 0;
pub const STDOUT: u8 = 1;
pub const STDERR: u8 = 2;
pub const EXIT: u8 = 3; // i32, big-endian
pub const RESIZE: u8 = 4; // rows u16, cols u16
pub const EOF: u8 = 5; // the caller's stdin ended
pub const STARTED: u8 = 6; // pid u32 in the berth
pub const ERROR: u8 = 7; // text

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ExecSpec {
    pub argv: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    pub cwd: Option<String>,
    pub user: Option<String>,
    #[serde(default)]
    pub tty: bool,
    #[serde(default)]
    pub rows: u16,
    #[serde(default)]
    pub cols: u16,
    #[serde(default)]
    pub detach: bool,
    pub name: Option<String>,
}

pub fn write_frame(w: &mut impl Write, kind: u8, data: &[u8]) -> io::Result<()> {
    let mut buf = Vec::with_capacity(5 + data.len());
    buf.push(kind);
    buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
    buf.extend_from_slice(data);
    w.write_all(&buf)
}

/// The next frame, or None when the stream ended cleanly.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut head = [0u8; 5];
    match r.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(head[1..].try_into().unwrap()) as usize;
    if len > 16 << 20 {
        return Err(io::Error::other("frame too large"));
    }
    let mut data = vec![0u8; len];
    r.read_exact(&mut data)?;
    Ok(Some((head[0], data)))
}

/// A frame writer several threads share.
#[derive(Clone)]
pub struct Frames(pub Arc<Mutex<UnixStream>>);

impl Frames {
    pub fn new(s: UnixStream) -> Self {
        Frames(Arc::new(Mutex::new(s)))
    }
    pub fn send(&self, kind: u8, data: &[u8]) -> io::Result<()> {
        write_frame(&mut *self.0.lock().unwrap(), kind, data)
    }
}

pub struct Request {
    pub method: String,
    pub path: String,
    pub upgrade: bool,
    pub body: Vec<u8>,
}

/// Reads up to the blank line after the headers, one byte at a time, so nothing
/// that follows an upgrade is swallowed by a buffer.
fn read_head(s: &mut UnixStream) -> anyhow::Result<String> {
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if s.read(&mut b)? == 0 {
            bail!("connection closed");
        }
        head.push(b[0]);
        if head.len() > 64 << 10 {
            bail!("headers too large");
        }
    }
    Ok(String::from_utf8_lossy(&head).into_owned())
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

pub fn read_request(s: &mut UnixStream) -> anyhow::Result<Request> {
    let head = read_head(s)?;
    let mut first = head.lines().next().unwrap_or("").split_whitespace();
    let method = first.next().unwrap_or("").to_string();
    let path = first.next().unwrap_or("").to_string();
    let len: usize = header(&head, "content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let upgrade = header(&head, "upgrade").is_some();
    let mut body = vec![0u8; len];
    s.read_exact(&mut body)?;
    Ok(Request { method, path, upgrade, body })
}

pub fn respond(s: &mut UnixStream, status: u16, body: &serde_json::Value) -> io::Result<()> {
    let body = serde_json::to_vec(body).unwrap();
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        502 => "Bad Gateway",
        _ => "Error",
    };
    write!(
        s,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    s.write_all(&body)
}

pub fn respond_upgrade(s: &mut UnixStream) -> io::Result<()> {
    s.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: berth\r\nConnection: Upgrade\r\n\r\n")
}

/// Sends a request to the daemon. For an upgrade, returns the connection once
/// the daemon switched protocols; otherwise the JSON body of a 2xx answer.
pub enum Answer {
    Json(serde_json::Value),
    Upgraded(UnixStream),
}

pub fn call(method: &str, path: &str, body: Option<&serde_json::Value>, upgrade: bool) -> anyhow::Result<Answer> {
    let mut s = UnixStream::connect(SOCKET).with_context(|| format!("connecting to berthd at {SOCKET}"))?;
    let body = body.map(|b| serde_json::to_vec(b).unwrap()).unwrap_or_default();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: berth\r\nContent-Length: {}\r\n", body.len());
    if upgrade {
        req.push_str("Connection: Upgrade\r\nUpgrade: berth\r\n");
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes())?;
    s.write_all(&body)?;
    let head = read_head(&mut s)?;
    let status: u16 = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    if status == 101 {
        return Ok(Answer::Upgraded(s));
    }
    let len: usize = header(&head, "content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut data = vec![0u8; len];
    s.read_exact(&mut data)?;
    let v: serde_json::Value = serde_json::from_slice(&data).unwrap_or(serde_json::Value::Null);
    if !(200..300).contains(&status) {
        bail!("{}", v.get("error").and_then(|e| e.as_str()).unwrap_or("request failed"));
    }
    Ok(Answer::Json(v))
}

pub fn call_json(method: &str, path: &str, body: Option<&serde_json::Value>) -> anyhow::Result<serde_json::Value> {
    match call(method, path, body, false)? {
        Answer::Json(v) => Ok(v),
        Answer::Upgraded(_) => bail!("unexpected upgrade"),
    }
}

pub fn call_upgrade(path: &str, body: &serde_json::Value) -> anyhow::Result<UnixStream> {
    match call("POST", path, Some(body), true)? {
        Answer::Upgraded(s) => Ok(s),
        Answer::Json(_) => bail!("the daemon didn't upgrade the connection"),
    }
}
