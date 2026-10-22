use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use anyhow::{Context, bail};
use serde_json::Value;

use super::http;

/// A client of the daemon's socket.
pub struct Client {
    socket: PathBuf,
}

impl Default for Client {
    fn default() -> Self {
        Client::new(super::socket_path())
    }
}

impl Client {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Client { socket: socket.into() }
    }

    fn send(&self, method: &str, path: &str, body: Option<&Value>, upgrade: bool) -> anyhow::Result<UnixStream> {
        let mut s = UnixStream::connect(&self.socket)
            .with_context(|| format!("connecting to berthd at {}", self.socket.display()))?;
        let body = body.map(|b| serde_json::to_vec(b).unwrap()).unwrap_or_default();
        http::write_request(&mut s, method, path, &body, upgrade)?;
        Ok(s)
    }

    /// Sends a request and returns the JSON body of a 2xx answer; an error
    /// answer becomes an error with the daemon's message.
    pub fn json(&self, method: &str, path: &str, body: Option<&Value>) -> anyhow::Result<Value> {
        let mut s = self.send(method, path, body, false)?;
        let resp = http::read_response(&mut s)?;
        let v: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
        if !(200..300).contains(&resp.status) {
            bail!("{}", v.get("error").and_then(|e| e.as_str()).unwrap_or("request failed"));
        }
        Ok(v)
    }

    /// Sends a request that switches the connection to a stream, and returns
    /// the connection once it has.
    pub fn upgrade(&self, path: &str, body: &Value) -> anyhow::Result<UnixStream> {
        let mut s = self.send("POST", path, Some(body), true)?;
        let resp = http::read_response(&mut s)?;
        match resp.status {
            101 => Ok(s),
            _ => {
                let v: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
                bail!(
                    "{}",
                    v.get("error").and_then(|e| e.as_str()).unwrap_or("the daemon didn't upgrade the connection")
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;

    /// A daemon stand-in that answers one request with `answer`.
    fn serve_once(answer: impl FnOnce(http::Request, &mut UnixStream) + Send + 'static) -> (tempfile::TempDir, Client) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("berth.sock");
        let listener = UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let req = http::read_request(&mut conn).unwrap();
            answer(req, &mut conn);
        });
        (dir, Client::new(path))
    }

    #[test]
    fn json_returns_the_body() {
        let (_dir, client) = serve_once(|req, conn| {
            assert_eq!((req.method.as_str(), req.path.as_str()), ("POST", "/v1/berths"));
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            http::respond(conn, 200, &json!({ "echo": body["image"] })).unwrap();
        });
        let v = client.json("POST", "/v1/berths", Some(&json!({ "image": "ubuntu:24.04" }))).unwrap();
        assert_eq!(v["echo"], "ubuntu:24.04");
    }

    #[test]
    fn errors_carry_the_daemons_message() {
        let (_dir, client) = serve_once(|_, conn| {
            http::respond(conn, 404, &json!({ "error": "no berth abc" })).unwrap();
        });
        let e = client.json("GET", "/v1/berths/abc", None).unwrap_err();
        assert_eq!(e.to_string(), "no berth abc");
    }

    #[test]
    fn upgrade_hands_over_the_stream() {
        let (_dir, client) = serve_once(|req, conn| {
            assert!(req.upgrade);
            http::respond_upgrade(conn).unwrap();
            let mut ping = [0u8; 4];
            conn.read_exact(&mut ping).unwrap();
            conn.write_all(b"pong").unwrap();
        });
        let mut s = client.upgrade("/v1/berths/x/exec", &json!({})).unwrap();
        s.write_all(b"ping").unwrap();
        let mut pong = [0u8; 4];
        s.read_exact(&mut pong).unwrap();
        assert_eq!(&pong, b"pong");
    }

    #[test]
    fn a_missing_daemon_says_where_it_looked() {
        let e = Client::new("/nonexistent/berth.sock").json("GET", "/v1/berths", None).unwrap_err();
        assert!(e.to_string().contains("/nonexistent/berth.sock"), "{e}");
    }
}
