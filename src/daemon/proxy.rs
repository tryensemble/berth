//! Reaching a berth's ports.

use std::fs::File;
use std::net::TcpStream;
use std::os::fd::AsRawFd;

use anyhow::{Context, anyhow};

use crate::sys;

/// Connects to port on the berth's own loopback, from inside its network
/// namespace, so servers bound to 127.0.0.1 are reachable too.
pub fn dial(pid: i32, port: u16) -> anyhow::Result<TcpStream> {
    let ns = File::open(format!("/proc/{pid}/ns/net"))?;
    std::thread::spawn(move || -> anyhow::Result<TcpStream> {
        // Only this short-lived thread enters the namespace; the socket stays
        // in it after the thread ends.
        sys::setns(ns.as_raw_fd(), libc::CLONE_NEWNET)?;
        TcpStream::connect(("127.0.0.1", port)).with_context(|| format!("nothing answers on port {port} in the berth"))
    })
    .join()
    .map_err(|_| anyhow!("dial thread panicked"))?
}
