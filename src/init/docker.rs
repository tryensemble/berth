//! Docker in a berth, started when it's first used.
//!
//! When the berth has dockerd, init listens on /run/docker.sock itself. The
//! first connection starts dockerd with that socket handed over (systemd's
//! socket activation, `-H fd://`), so the connection that woke it is served
//! and berths that never use Docker never pay for it. If dockerd exits, init
//! listens again.

use std::fs::{self, OpenOptions};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use super::passwd;
use super::reaper::Reaper;
use crate::sys;

const DOCKERD: &str = "/usr/bin/dockerd";
const SOCKET: &str = "/run/docker.sock";
/// The descriptor systemd's activation protocol hands the first socket on.
const LISTEN_FD: i32 = 3;

/// Settings for the berth's dockerd.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default)]
pub struct DockerConfig {
    /// A pull-through registry mirror for Docker Hub.
    pub registry_mirror: Option<String>,
}

/// Starts listening for Docker if the berth has it.
pub fn start(cfg: &DockerConfig, reaper: &Reaper) -> anyhow::Result<()> {
    if !Path::new(DOCKERD).exists() {
        return Ok(());
    }
    let _ = fs::remove_file(SOCKET);
    let listener = UnixListener::bind(SOCKET)?;
    let gid = passwd::group_id("docker").unwrap_or(0);
    sys::cvt(unsafe { libc::chown(c"/run/docker.sock".as_ptr(), 0, gid) })?;
    fs::set_permissions(SOCKET, fs::Permissions::from_mode(0o660))?;
    // Containers from an earlier run may have restart policies: start now.
    let eager = fs::read_dir("/var/lib/docker/containers").is_ok_and(|mut d| d.next().is_some());
    let (cfg, reaper) = (cfg.clone(), reaper.clone());
    std::thread::spawn(move || {
        let fd = OwnedFd::from(listener);
        let mut first = true;
        loop {
            if !(first && eager) {
                wait_readable(&fd);
            }
            first = false;
            match run(&cfg, &fd, &reaper) {
                Ok(code) => eprintln!("berth-init: dockerd exited with {code}"),
                Err(e) => eprintln!("berth-init: starting dockerd: {e:#}"),
            }
            // Don't spin if it can't stay up.
            std::thread::sleep(Duration::from_secs(1));
        }
    });
    Ok(())
}

fn wait_readable(fd: &OwnedFd) {
    let mut p = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    while unsafe { libc::poll(&mut p, 1, -1) } < 0 {}
}

/// Runs dockerd on the listening socket until it exits.
fn run(cfg: &DockerConfig, fd: &OwnedFd, reaper: &Reaper) -> anyhow::Result<i32> {
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o640)
        .custom_flags(libc::O_NOFOLLOW)
        .open("/var/log/berth/dockerd.log")?;
    let mut args = vec!["-H".to_string(), "fd://".into(), "--data-root".into(), "/var/lib/docker".into()];
    if let Some(m) = &cfg.registry_mirror {
        let host = m.trim_start_matches("http://").trim_start_matches("https://");
        args.extend(["--registry-mirror".into(), m.clone(), "--insecure-registry".into(), host.into()]);
    }
    // The activation protocol wants LISTEN_PID to be dockerd's own pid, which
    // only the shell that execs it knows.
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(r#"LISTEN_PID=$$ LISTEN_FDS=1 exec "$0" "$@""#)
        .arg(DOCKERD)
        .args(&args)
        .env_clear()
        .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    let raw = fd.as_raw_fd();
    unsafe {
        cmd.pre_exec(move || {
            libc::setsid();
            if raw == LISTEN_FD {
                sys::cvt(libc::fcntl(raw, libc::F_SETFD, 0))?;
            } else {
                sys::cvt(libc::dup2(raw, LISTEN_FD))?;
            }
            Ok(())
        });
    }
    let (_child, exited) = reaper.spawn(&mut cmd)?;
    Ok(exited.recv().unwrap_or(-1))
}
