//! Starting a berth's processes: its user namespace, its init, and commands
//! handed to that init.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use anyhow::{Context, anyhow, bail};

use super::UID_COUNT;
use super::berth::Berth;
use crate::api::{ExecSpec, frames};
use crate::init::{InitConfig, LISTEN_FD};
use crate::sys;

/// A user namespace mapping ids 0..65536 to base.., kept by its fd.
pub fn new_userns(base: u32) -> anyhow::Result<OwnedFd> {
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("__hold").stdin(Stdio::piped()).stdout(Stdio::null());
    unsafe {
        cmd.pre_exec(|| sys::cvt(libc::unshare(libc::CLONE_NEWUSER)).map(drop));
    }
    let mut child = cmd.spawn()?;
    let pid = child.id();
    let map = format!("0 {base} {UID_COUNT}\n");
    let result = (|| -> anyhow::Result<OwnedFd> {
        fs::write(format!("/proc/{pid}/uid_map"), &map)?;
        fs::write(format!("/proc/{pid}/gid_map"), &map)?;
        Ok(File::open(format!("/proc/{pid}/ns/user"))?.into())
    })();
    drop(child.stdin.take());
    let _ = child.wait();
    result
}

/// Starts berth-init in the berth's namespaces and cgroup. Returns the host pid
/// of the berth's PID 1, a pipe to tell it to go, and its output.
pub fn spawn_init(
    b: &Berth,
    cfg: &InitConfig,
    userns: &OwnedFd,
    listener: &UnixListener,
) -> anyhow::Result<(i32, std::process::ChildStdin, BufReader<std::process::ChildStdout>)> {
    let procs = File::options().write(true).open(b.cgroup().join("cgroup.procs"))?;
    let log = File::create(b.dir().join("init.log"))?;
    let (pfd, ufd, lfd) = (procs.as_raw_fd(), userns.as_raw_fd(), listener.as_raw_fd());
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("__init")
        .env_clear()
        .env("BERTH_INIT_CONFIG", serde_json::to_string(cfg)?)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(log);
    unsafe {
        cmd.pre_exec(move || {
            // Join the cgroup first: after entering the user namespace this
            // process can't move itself any more.
            let mut buf = [0u8; 24];
            let pid = sys::itoa(libc::getpid() as u64, &mut buf);
            if libc::write(pfd, pid.as_ptr() as *const _, pid.len()) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            sys::cvt(libc::setns(ufd, libc::CLONE_NEWUSER))?;
            sys::cvt(libc::setgroups(0, std::ptr::null()))?;
            sys::cvt(libc::setresgid(0, 0, 0))?;
            sys::cvt(libc::setresuid(0, 0, 0))?;
            sys::cvt(libc::unshare(
                libc::CLONE_NEWNS
                    | libc::CLONE_NEWPID
                    | libc::CLONE_NEWNET
                    | libc::CLONE_NEWIPC
                    | libc::CLONE_NEWUTS
                    | libc::CLONE_NEWCGROUP,
            ))?;
            if lfd == LISTEN_FD {
                sys::cvt(libc::fcntl(lfd, libc::F_SETFD, 0))?;
            } else {
                sys::cvt(libc::dup2(lfd, LISTEN_FD))?;
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("starting berth-init")?;
    let go = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    out.read_line(&mut line)?;
    let _ = child.wait();
    let pid: i32 = line.trim().parse().map_err(|_| {
        anyhow!("berth-init failed: {}", fs::read_to_string(b.dir().join("init.log")).unwrap_or_default().trim())
    })?;
    Ok((pid, go, out))
}

/// Hands an exec request and the stream to speak frames on to the berth's init.
pub fn hand_to_init(b: &Berth, spec: &ExecSpec, stream_fd: i32) -> anyhow::Result<()> {
    let control = UnixStream::connect(b.run_dir().join("init.sock")).context("reaching the berth's init")?;
    sys::send_with_fd(&control, &serde_json::to_vec(spec)?, stream_fd)?;
    Ok(())
}

/// Runs a command in a berth and collects its exit code and output.
pub fn exec_collect(b: &Berth, spec: &ExecSpec) -> anyhow::Result<(i32, String)> {
    let (mut mine, theirs) = UnixStream::pair()?;
    hand_to_init(b, spec, theirs.as_raw_fd())?;
    drop(theirs);
    let mut output = Vec::new();
    while let Some((kind, data)) = frames::read_frame(&mut mine)? {
        match kind {
            frames::STDOUT | frames::STDERR | frames::ERROR => output.extend_from_slice(&data),
            frames::EXIT if data.len() == 4 => {
                return Ok((i32::from_be_bytes(data.try_into().unwrap()), String::from_utf8_lossy(&output).into()));
            }
            _ => {}
        }
    }
    bail!("the berth's init hung up: {}", String::from_utf8_lossy(&output))
}

/// What `berth __hold` does: keeps a namespace alive until stdin closes.
pub fn hold() {
    let mut buf = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut buf);
}
