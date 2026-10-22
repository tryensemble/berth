//! berth-init: the first process of a berth. The daemon starts it already in
//! the berth's user namespace and with fresh mount, PID, network, IPC, UTS and
//! cgroup namespaces; it forks the berth's PID 1, which builds the berth's
//! filesystem, pivots into it, and from then on starts every command run in the
//! berth. Commands are its children, not the daemon's, so they outlive the
//! daemon.

pub mod docker;
mod exec;
mod passwd;
mod reaper;
mod rootfs;
mod services;

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use crate::api::{Frames, InitRequest, frames};
use crate::sys;
use reaper::Reaper;
use services::Services;

/// The descriptor init finds its control socket on.
pub const LISTEN_FD: i32 = 3;

/// What the daemon tells init about the berth it starts.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct InitConfig {
    pub id: String,
    pub hostname: String,
    /// The berth's root filesystem, an overlay the daemon mounted.
    pub root: PathBuf,
    pub workspace: PathBuf,
    pub dns: Vec<String>,
    pub tmp_size: String,
    /// More directories to mount in the berth.
    #[serde(default)]
    pub binds: Vec<Bind>,
    #[serde(default)]
    pub docker: docker::DockerConfig,
}

/// A host directory mounted in the berth.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Bind {
    pub source: PathBuf,
    /// Where, inside the berth.
    pub target: String,
    #[serde(default)]
    pub read_only: bool,
}

pub fn main() -> anyhow::Result<()> {
    let cfg: InitConfig = serde_json::from_str(&std::env::var("BERTH_INIT_CONFIG")?)?;
    // This process is outside the new PID namespace; its first child is PID 1.
    match unsafe { libc::fork() } {
        -1 => bail!("fork: {}", std::io::Error::last_os_error()),
        0 => {
            if let Err(e) = pid1(cfg) {
                eprintln!("berth-init: {e:#}");
                std::process::exit(1);
            }
            std::process::exit(0);
        }
        pid => {
            println!("{pid}");
            std::io::stdout().flush()?;
            Ok(())
        }
    }
}

fn pid1(cfg: InitConfig) -> anyhow::Result<()> {
    // The daemon says go once the berth's network is in place.
    let mut line = String::new();
    BufReader::new(std::io::stdin()).read_line(&mut line)?;
    if line.trim() != "go" {
        bail!("the daemon didn't say go");
    }
    rootfs::build_root(&cfg).context("building the berth's filesystem")?;
    let host = std::ffi::CString::new(cfg.hostname.clone())?;
    sys::cvt(unsafe { libc::sethostname(host.as_ptr(), cfg.hostname.len()) })?;

    println!("ready");
    std::io::stdout().flush()?;
    let null = File::options().read(true).write(true).open("/dev/null")?;
    unsafe {
        libc::dup2(null.as_raw_fd(), 0);
        libc::dup2(null.as_raw_fd(), 1);
    }

    // From here on, this process and everything it starts are under the
    // berth's seccomp filter.
    sys::seccomp::install(&sys::seccomp::program()).context("installing the seccomp filter")?;

    let reaper = Reaper::start();
    let services = Services::default();
    if let Err(e) = docker::start(&cfg.docker, &reaper) {
        eprintln!("berth-init: no Docker: {e:#}");
    }
    // The control socket is init's alone: nothing it starts may inherit it.
    sys::cvt(unsafe { libc::fcntl(LISTEN_FD, libc::F_SETFD, libc::FD_CLOEXEC) })?;
    let listener = unsafe { UnixListener::from_raw_fd(LISTEN_FD) };
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let (reaper, services) = (reaper.clone(), services.clone());
        std::thread::spawn(move || {
            if let Err(e) = handle(conn, reaper, services) {
                eprintln!("berth-init: {e:#}");
            }
        });
    }
    Ok(())
}

fn handle(control: UnixStream, reaper: Reaper, services: Services) -> anyhow::Result<()> {
    let (msg, fd) = sys::recv_with_fd(&control)?;
    match serde_json::from_slice(&msg)? {
        InitRequest::Exec(spec) => {
            drop(control);
            let stream = UnixStream::from(fd.context("no stream attached")?);
            let frames = Frames::new(stream.try_clone()?);
            let result = match spec.service {
                Some(_) => {
                    services.start(spec, &reaper).and_then(|pid| Ok(frames.send(frames::STARTED, &pid.to_be_bytes())?))
                }
                None => exec::run(spec, stream, frames.clone(), reaper),
            };
            if let Err(e) = result {
                let _ = frames.send(frames::ERROR, format!("{e:#}").as_bytes());
                let _ = frames.send(frames::EXIT, &127i32.to_be_bytes());
            }
        }
        InitRequest::StopService(name) => {
            services.stop(&name);
            (&control).write_all(b"ok\n")?;
        }
    }
    Ok(())
}
