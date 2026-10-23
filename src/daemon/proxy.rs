//! Reaching a berth's ports: one connection at a time over the API, through a
//! SOCKS5 socket of the berth's own, or published on a host port.
//!
//! Every connection is opened from inside the berth's network namespace, so
//! `localhost` is the berth's own loopback (a dev server bound to 127.0.0.1 is
//! reachable) and the berth's network rules apply to anything else.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{PermissionsExt, chown};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use serde::{Deserialize, Serialize};

use super::{Daemon, paths};
use crate::{pipe, sys};

/// How long a connection into a berth may take to be accepted.
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

/// Connects to port on the berth's own loopback.
pub fn dial(pid: i32, port: u16) -> anyhow::Result<TcpStream> {
    dial_to(pid, SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
}

/// Connects to addr from inside the berth whose PID 1 is pid.
pub fn dial_to(pid: i32, addr: SocketAddr) -> anyhow::Result<TcpStream> {
    let ns = File::open(format!("/proc/{pid}/ns/net"))?;
    std::thread::spawn(move || -> anyhow::Result<TcpStream> {
        // Only this short-lived thread enters the namespace; the socket stays
        // in it after the thread ends.
        sys::setns(ns.as_raw_fd(), libc::CLONE_NEWNET)?;
        TcpStream::connect_timeout(&addr, DIAL_TIMEOUT).with_context(|| match addr.ip() {
            IpAddr::V4(ip) if ip.is_loopback() => format!("nothing answers on port {} in the berth", addr.port()),
            _ => format!("the berth can't reach {addr}"),
        })
    })
    .join()
    .map_err(|_| anyhow!("dial thread panicked"))?
}

/// A host port that forwards to a port in a berth.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Published {
    /// The port in the berth.
    pub port: u16,
    /// Where on the host it's served.
    pub address: IpAddr,
    pub host_port: u16,
}

/// Listeners the daemon serves for berths, by key, so they can be closed.
#[derive(Default)]
pub struct Listeners(Mutex<HashMap<String, OwnedFd>>);

impl Listeners {
    fn add(&self, key: String, fd: OwnedFd) {
        if let Some(old) = self.0.lock().unwrap().insert(key, fd) {
            close(&old);
        }
    }

    fn remove(&self, key: &str) {
        if let Some(fd) = self.0.lock().unwrap().remove(key) {
            close(&fd);
        }
    }

    /// Closes every listener whose key starts with prefix.
    fn remove_all(&self, prefix: &str) {
        let mut all = self.0.lock().unwrap();
        let keys: Vec<String> = all.keys().filter(|k| k.starts_with(prefix)).cloned().collect();
        for k in keys {
            if let Some(fd) = all.remove(&k) {
                close(&fd);
            }
        }
    }
}

/// Wakes the thread blocked in accept() on fd, which then ends.
fn close(fd: &OwnedFd) {
    unsafe { libc::shutdown(fd.as_raw_fd(), libc::SHUT_RDWR) };
}

/// Where a berth's SOCKS5 socket is.
pub fn socks_path(id: &str) -> std::path::PathBuf {
    paths::run("socks").join(format!("{id}.sock"))
}

impl Daemon {
    /// Serves the berth's SOCKS5 socket and its published ports. Called when
    /// a berth is made and for every berth when the daemon starts.
    pub fn expose(self: &Arc<Self>, id: &str) {
        if let Err(e) = self.serve_socks(id) {
            eprintln!("berthd: berth {id}: SOCKS socket: {e:#}");
        }
        let published = self.get(id).map(|b| b.published).unwrap_or_default();
        for p in published {
            if let Err(e) = self.listen_published(id, &p) {
                eprintln!("berthd: berth {id}: publishing port {}: {e:#}", p.port);
            }
        }
    }

    /// Stops serving the berth's ports.
    pub fn unexpose(&self, id: &str) {
        self.listeners.remove(&format!("socks:{id}"));
        self.listeners.remove_all(&format!("publish:{id}:"));
        let _ = std::fs::remove_file(socks_path(id));
    }

    fn serve_socks(self: &Arc<Self>, id: &str) -> anyhow::Result<()> {
        let dir = paths::run("socks");
        std::fs::create_dir_all(&dir)?;
        let gid = super::group_id("berth");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o750))?;
        chown(&dir, Some(0), gid)?;
        let path = socks_path(id);
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660))?;
        chown(&path, Some(0), gid)?;
        self.listeners.add(format!("socks:{id}"), OwnedFd::from(listener.try_clone()?));
        let (d, id) = (self.clone(), id.to_string());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { break };
                let (d, id) = (d.clone(), id.clone());
                std::thread::spawn(move || {
                    if let Err(e) = d.socks(&id, conn) {
                        eprintln!("berthd: berth {id}: SOCKS: {e:#}");
                    }
                });
            }
        });
        Ok(())
    }

    /// Serves one SOCKS5 connection: no authentication (the socket's
    /// permissions are the access control), CONNECT only.
    fn socks(self: &Arc<Self>, id: &str, mut conn: UnixStream) -> anyhow::Result<()> {
        conn.set_read_timeout(Some(DIAL_TIMEOUT))?;
        let target = match socks_handshake(&mut conn) {
            Ok(t) => t,
            Err(e) => {
                let _ = conn.write_all(&socks_reply(e.code));
                bail!("{}", e.message);
            }
        };
        let _busy = self.stream(id);
        let result = (|| -> anyhow::Result<TcpStream> {
            let b = self.wake(id)?;
            let addr = resolve(&target)?;
            dial_to(b.pid, addr)
        })();
        let tcp = match result {
            Ok(t) => t,
            Err(e) => {
                let _ = conn.write_all(&socks_reply(REPLY_REFUSED));
                return Err(e);
            }
        };
        conn.write_all(&socks_reply(REPLY_OK))?;
        conn.set_read_timeout(None)?;
        pipe::splice(conn, tcp)?;
        Ok(())
    }

    fn listen_published(self: &Arc<Self>, id: &str, p: &Published) -> anyhow::Result<()> {
        let listener = TcpListener::bind((p.address, p.host_port))
            .with_context(|| format!("listening on {}:{}", p.address, p.host_port))?;
        self.listeners.add(format!("publish:{id}:{}", p.port), OwnedFd::from(listener.try_clone()?));
        let (d, id, port) = (self.clone(), id.to_string(), p.port);
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { break };
                let (d, id) = (d.clone(), id.clone());
                std::thread::spawn(move || {
                    let _busy = d.stream(&id);
                    match d.wake(&id).and_then(|b| dial(b.pid, port)) {
                        Ok(tcp) => {
                            let _ = pipe::splice(conn, tcp);
                        }
                        Err(e) => {
                            let _ = conn.shutdown(Shutdown::Both);
                            eprintln!("berthd: berth {id}: published port {port}: {e:#}");
                        }
                    }
                });
            }
        });
        Ok(())
    }

    /// Publishes the berth's port on the host, from now on and after restarts.
    pub fn publish(
        self: &Arc<Self>,
        id: &str,
        port: u16,
        address: IpAddr,
        host_port: u16,
    ) -> anyhow::Result<Published> {
        let lock = self.lock_of(id);
        let _held = lock.lock().unwrap();
        let mut b = self.get(id)?;
        if b.published.iter().any(|p| p.port == port) {
            bail!("berth {id} already publishes port {port}");
        }
        let p = Published { port, address, host_port };
        self.listen_published(id, &p)?;
        b.published.push(p.clone());
        self.put(&b)?;
        Ok(p)
    }

    pub fn unpublish(&self, id: &str, port: u16) -> anyhow::Result<()> {
        let lock = self.lock_of(id);
        let _held = lock.lock().unwrap();
        let mut b = self.get(id)?;
        let before = b.published.len();
        b.published.retain(|p| p.port != port);
        if b.published.len() == before {
            bail!("berth {id} doesn't publish port {port}");
        }
        self.listeners.remove(&format!("publish:{id}:{port}"));
        self.put(&b)
    }
}

const REPLY_OK: u8 = 0;
const REPLY_FAILURE: u8 = 1;
const REPLY_REFUSED: u8 = 5;
const REPLY_COMMAND: u8 = 7;
const REPLY_ADDRESS: u8 = 8;

/// Where a SOCKS client asked to go.
#[derive(Debug, PartialEq)]
pub enum Target {
    Addr(SocketAddr),
    Name(String, u16),
}

#[derive(Debug)]
pub struct SocksError {
    code: u8,
    message: String,
}

fn fail(code: u8, message: impl Into<String>) -> SocksError {
    SocksError { code, message: message.into() }
}

/// Reads a SOCKS5 greeting and CONNECT request.
pub fn socks_handshake(s: &mut impl ReadWrite) -> Result<Target, SocksError> {
    let io = |e: std::io::Error| fail(REPLY_FAILURE, e.to_string());
    let mut head = [0u8; 2];
    s.read_exact(&mut head).map_err(io)?;
    if head[0] != 5 {
        return Err(fail(REPLY_FAILURE, "not SOCKS5"));
    }
    let mut methods = vec![0u8; head[1] as usize];
    s.read_exact(&mut methods).map_err(io)?;
    if !methods.contains(&0) {
        s.write_all(&[5, 0xff]).map_err(io)?;
        return Err(fail(REPLY_FAILURE, "the client offers no method without authentication"));
    }
    s.write_all(&[5, 0]).map_err(io)?;

    let mut req = [0u8; 4];
    s.read_exact(&mut req).map_err(io)?;
    if req[0] != 5 {
        return Err(fail(REPLY_FAILURE, "not SOCKS5"));
    }
    if req[1] != 1 {
        return Err(fail(REPLY_COMMAND, "only CONNECT is supported"));
    }
    let mut port = [0u8; 2];
    let target = match req[3] {
        1 => {
            let mut a = [0u8; 4];
            s.read_exact(&mut a).map_err(io)?;
            s.read_exact(&mut port).map_err(io)?;
            Target::Addr(SocketAddr::from((Ipv4Addr::from(a), u16::from_be_bytes(port))))
        }
        4 => {
            let mut a = [0u8; 16];
            s.read_exact(&mut a).map_err(io)?;
            s.read_exact(&mut port).map_err(io)?;
            Target::Addr(SocketAddr::from((Ipv6Addr::from(a), u16::from_be_bytes(port))))
        }
        3 => {
            let mut len = [0u8; 1];
            s.read_exact(&mut len).map_err(io)?;
            let mut name = vec![0u8; len[0] as usize];
            s.read_exact(&mut name).map_err(io)?;
            s.read_exact(&mut port).map_err(io)?;
            let name = String::from_utf8(name).map_err(|_| fail(REPLY_ADDRESS, "a host name that isn't text"))?;
            Target::Name(name, u16::from_be_bytes(port))
        }
        _ => return Err(fail(REPLY_ADDRESS, "unknown address type")),
    };
    Ok(target)
}

/// A reply to a CONNECT, with no bound address (clients don't use it).
fn socks_reply(code: u8) -> [u8; 10] {
    [5, code, 0, 1, 0, 0, 0, 0, 0, 0]
}

/// Where target is: `localhost` is the berth's own; other names are looked
/// up on the host, and the berth's rules decide whether it may go there.
pub fn resolve(target: &Target) -> anyhow::Result<SocketAddr> {
    match target {
        Target::Addr(a) => Ok(*a),
        Target::Name(n, port) if n.eq_ignore_ascii_case("localhost") => {
            Ok(SocketAddr::from((Ipv4Addr::LOCALHOST, *port)))
        }
        Target::Name(n, port) => (n.as_str(), *port)
            .to_socket_addrs()?
            .find(|a| a.is_ipv4())
            .with_context(|| format!("no IPv4 address for {n}")),
    }
}

/// Read + Write, so the handshake can be tested over a buffer.
pub trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

#[cfg(test)]
mod tests {
    use super::*;

    /// A client's bytes in, the server's bytes out.
    struct Wire {
        input: std::io::Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl Read for Wire {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.input.read(buf)
        }
    }

    impl Write for Wire {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.output.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn wire(bytes: &[u8]) -> Wire {
        Wire { input: std::io::Cursor::new(bytes.to_vec()), output: Vec::new() }
    }

    #[test]
    fn connects_by_name_ipv4_and_ipv6() {
        let mut w = wire(&[5, 1, 0, 5, 1, 0, 3, 9, b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't', 0x0b, 0xb8]);
        assert_eq!(socks_handshake(&mut w).unwrap(), Target::Name("localhost".into(), 3000));
        assert_eq!(w.output, [5, 0]);

        let mut w = wire(&[5, 1, 0, 5, 1, 0, 1, 127, 0, 0, 1, 0x15, 0x38]);
        assert_eq!(socks_handshake(&mut w).unwrap(), Target::Addr("127.0.0.1:5432".parse().unwrap()));

        let mut req = vec![5, 1, 0, 5, 1, 0, 4];
        req.extend_from_slice(&Ipv6Addr::LOCALHOST.octets());
        req.extend_from_slice(&80u16.to_be_bytes());
        assert_eq!(socks_handshake(&mut wire(&req)).unwrap(), Target::Addr("[::1]:80".parse().unwrap()));
    }

    #[test]
    fn only_connect_without_authentication() {
        let mut w = wire(&[5, 1, 2]);
        assert!(socks_handshake(&mut w).is_err());
        assert_eq!(w.output, [5, 0xff]);
        // BIND
        let e = socks_handshake(&mut wire(&[5, 1, 0, 5, 2, 0, 1, 127, 0, 0, 1, 0, 80])).unwrap_err();
        assert_eq!(e.code, REPLY_COMMAND);
        let e = socks_handshake(&mut wire(&[4, 1, 0, 80])).unwrap_err();
        assert_eq!(e.code, REPLY_FAILURE);
        let e = socks_handshake(&mut wire(&[5, 1, 0, 5, 1, 0, 9])).unwrap_err();
        assert_eq!(e.code, REPLY_ADDRESS);
    }

    #[test]
    fn localhost_is_the_berths_own() {
        let a = resolve(&Target::Name("LocalHost".into(), 3000)).unwrap();
        assert_eq!(a, "127.0.0.1:3000".parse().unwrap());
    }
}
