//! Reaching a berth's ports from outside: its SOCKS5 socket and published
//! ports.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use crate::{Berth, berth, ok};

/// Fetches / from host:port through a SOCKS5 socket; None if refused.
fn socks_get(socket: &str, host: &str, port: u16) -> Option<String> {
    let mut s = UnixStream::connect(socket).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    s.write_all(&[5, 1, 0]).unwrap();
    let mut greeting = [0u8; 2];
    s.read_exact(&mut greeting).unwrap();
    assert_eq!(greeting, [5, 0]);
    let mut req = vec![5, 1, 0, 3, host.len() as u8];
    req.extend_from_slice(host.as_bytes());
    req.extend_from_slice(&port.to_be_bytes());
    s.write_all(&req).unwrap();
    let mut reply = [0u8; 10];
    s.read_exact(&mut reply).unwrap();
    if reply[1] != 0 {
        return None;
    }
    s.write_all(b"GET / HTTP/1.0\r\n\r\n").unwrap();
    let mut body = String::new();
    s.read_to_string(&mut body).unwrap();
    body.lines().last().map(String::from)
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn the_socks_socket_reaches_the_berths_own_localhost() {
    let b = Berth::new(&[]);
    b.serve("127.0.0.1", 3000, "on loopback");
    let socket = ok(&["socks", &b.0]);
    let mode = std::fs::metadata(&socket).map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions())).unwrap();
    assert_eq!(mode & 0o777, 0o660, "the SOCKS socket is open to more than the berth group");
    assert_eq!(socks_get(&socket, "localhost", 3000).as_deref(), Some("on loopback"));
    // The berth's own network rules still hold: the host isn't reachable.
    assert_eq!(socks_get(&socket, "10.200.0.1", 22), None);
    // Nor another berth.
    let other = Berth::new(&[]);
    other.serve("0.0.0.0", 3000, "other");
    let other_ip = other.info()["ip"].as_str().unwrap().to_string();
    assert_eq!(socks_get(&socket, &other_ip, 3000), None);
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn a_published_port_serves_and_wakes_the_berth() {
    let b = Berth::new(&[]);
    b.serve("127.0.0.1", 3000, "published");
    let host_port = 20000 + (std::process::id() % 20000) as u16;
    ok(&["publish", &b.0, "3000", "--host-port", &host_port.to_string()]);
    let fetch = || {
        let mut s = TcpStream::connect(("127.0.0.1", host_port)).ok()?;
        s.write_all(b"GET / HTTP/1.0\r\n\r\n").ok()?;
        let mut body = String::new();
        s.read_to_string(&mut body).ok()?;
        body.lines().last().map(String::from)
    };
    assert_eq!(fetch().as_deref(), Some("published"));
    ok(&["idle", &b.0]);
    assert_eq!(fetch().as_deref(), Some("published"));
    assert_eq!(b.status(), "running");
    assert!(!berth(&["publish", &b.0, "3000"]).status.success(), "a port was published twice");

    ok(&["unpublish", &b.0, "3000"]);
    assert!(TcpStream::connect(("127.0.0.1", host_port)).is_err(), "an unpublished port still answers");
}
