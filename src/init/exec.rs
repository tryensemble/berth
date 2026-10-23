//! Running a command in the berth for a caller: with pipes or a terminal, or
//! detached with its output in a log.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, bail};

use super::passwd;
use super::reaper::Reaper;
use crate::api::{ExecSpec, Frames, frames};
use crate::sys;

/// Runs spec for a caller on stream: attached with pipes or a terminal, or
/// detached, in which case only its pid is sent.
pub fn run(spec: ExecSpec, stream: UnixStream, frames: Frames, reaper: Reaper) -> anyhow::Result<()> {
    if spec.detach {
        let (child, _) = spawn_detached(&spec, &reaper)?;
        frames.send(frames::STARTED, &child.id().to_be_bytes())?;
        return Ok(());
    }
    let cmd = command(&spec)?;
    if spec.tty { run_tty(spec, cmd, stream, frames, reaper) } else { run_pipes(cmd, stream, frames, reaper) }
}

/// Starts spec in its own session with its output appended to
/// /var/log/berth/<name>.log.
pub fn spawn_detached(spec: &ExecSpec, reaper: &Reaper) -> anyhow::Result<(Child, mpsc::Receiver<i32>)> {
    let name = spec.service.as_deref().or(spec.name.as_deref()).unwrap_or("detached");
    let r = resolve(spec)?;
    let mut cmd = build(&r)?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW)
        .open(format!("/var/log/berth/{}.log", name.replace('/', "_")))?;
    unsafe { libc::fchown(log.as_raw_fd(), r.uid, r.gid) };
    cmd.stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    Ok(reaper.spawn(&mut cmd)?)
}

/// A command resolved to exactly what should run: the program and its
/// arguments, who as, where, and with what environment.
pub struct Resolved {
    pub argv: Vec<String>,
    pub env: Vec<String>,
    pub cwd: String,
    pub uid: u32,
    pub gid: u32,
    pub groups: Vec<u32>,
    pub strict: bool,
}

fn command(spec: &ExecSpec) -> anyhow::Result<Command> {
    build(&resolve(spec)?)
}

/// The command for r: run directly, or under gVisor when the berth is strict.
fn build(r: &Resolved) -> anyhow::Result<Command> {
    if r.strict {
        return super::strict::command(r);
    }
    let mut cmd = Command::new(&r.argv[0]);
    cmd.args(&r.argv[1..]).env_clear().envs(r.env.iter().filter_map(|kv| kv.split_once('='))).current_dir(&r.cwd);
    // std would drop supplementary groups; the agent needs its own (sudo,
    // docker), so the credentials are set here instead.
    let (uid, gid, groups) = (r.uid, r.gid, r.groups.clone());
    unsafe {
        cmd.pre_exec(move || {
            sys::cvt(libc::setgroups(groups.len(), groups.as_ptr()))?;
            sys::cvt(libc::setresgid(gid, gid, gid))?;
            sys::cvt(libc::setresuid(uid, uid, uid))?;
            Ok(())
        });
    }
    Ok(cmd)
}

/// Resolves spec against the berth's users and defaults.
fn resolve(spec: &ExecSpec) -> anyhow::Result<Resolved> {
    if spec.argv.is_empty() {
        bail!("no command");
    }
    let wanted = spec
        .user
        .clone()
        .unwrap_or_else(|| if passwd::lookup("agent").is_some() { "agent".into() } else { "root".into() });
    let user = passwd::lookup(&wanted).with_context(|| format!("no user {wanted} in the berth"))?;

    let path = if user.uid == 0 {
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string()
    } else {
        format!("{}/.local/bin:/usr/local/bin:/usr/bin:/bin:/usr/local/sbin:/usr/sbin:/sbin", user.home)
    };
    let mut env = vec![
        format!("PATH={path}"),
        format!("HOME={}", user.home),
        format!("USER={}", user.name),
        format!("LOGNAME={}", user.name),
        format!("SHELL={}", user.shell),
        "LANG=C.UTF-8".to_string(),
    ];
    if spec.tty {
        env.push("TERM=xterm-256color".into());
    }
    // The caller's own settings go last, so they win.
    for kv in &spec.env {
        if kv.contains('=') {
            let key = kv.split('=').next().unwrap();
            env.retain(|e| !e.starts_with(&format!("{key}=")));
            env.push(kv.clone());
        }
    }
    let cwd = spec.cwd.clone().unwrap_or_else(|| {
        if Path::new("/workspace/.git").exists() || user.uid != 0 { "/workspace".into() } else { "/".into() }
    });
    Ok(Resolved {
        argv: spec.argv.clone(),
        env,
        cwd,
        uid: user.uid,
        gid: user.gid,
        groups: passwd::groups(&user),
        strict: strict_mode(),
    })
}

/// Whether this berth runs commands under gVisor.
fn strict_mode() -> bool {
    std::env::var("BERTH_STRICT").is_ok_and(|v| v == "1")
}

fn exit_after(frames: &Frames, rx: mpsc::Receiver<i32>, outputs: mpsc::Receiver<()>, n: usize) -> anyhow::Result<()> {
    let code = rx.recv().unwrap_or(255);
    // Let the output drain; a background grandchild may hold it open forever.
    for _ in 0..n {
        if outputs.recv_timeout(Duration::from_millis(500)).is_err() {
            break;
        }
    }
    frames.send(frames::EXIT, &code.to_be_bytes())?;
    frames.shutdown();
    Ok(())
}

fn run_pipes(mut cmd: Command, stream: UnixStream, frames: Frames, reaper: Reaper) -> anyhow::Result<()> {
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let (mut child, rx) = reaper.spawn(&mut cmd)?;
    let pid = child.id() as i32;
    frames.send(frames::STARTED, &(pid as u32).to_be_bytes())?;

    let (done_tx, done_rx) = mpsc::channel();
    for (out, kind) in [
        (Box::new(child.stdout.take().unwrap()) as Box<dyn Read + Send>, frames::STDOUT),
        (Box::new(child.stderr.take().unwrap()), frames::STDERR),
    ] {
        pump_out(out, kind, frames.clone(), done_tx.clone());
    }
    let mut stdin = child.stdin.take();
    let mut input = stream;
    std::thread::spawn(move || {
        loop {
            match frames::read_frame(&mut input) {
                Ok(Some((frames::STDIN, data))) => {
                    if let Some(s) = stdin.as_mut() {
                        let _ = s.write_all(&data);
                    }
                }
                Ok(Some((frames::EOF, _))) => stdin = None,
                Ok(Some(_)) => {}
                _ => {
                    // The caller is gone: so is what it ran.
                    unsafe { libc::kill(-pid, libc::SIGKILL) };
                    break;
                }
            }
        }
    });
    exit_after(&frames, rx, done_rx, 2)
}

fn pump_out(mut out: Box<dyn Read + Send>, kind: u8, frames: Frames, done: mpsc::Sender<()>) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 32 << 10];
        loop {
            match out.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if frames.send(kind, &buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = done.send(());
    });
}

/// A new pty's two ends, both close-on-exec from the start: init runs
/// commands from several threads, and one spawned meanwhile mustn't inherit
/// another session's terminal.
fn open_pty() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let flags = libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC;
    let master = unsafe { OwnedFd::from_raw_fd(sys::cvt(libc::posix_openpt(flags))?) };
    sys::cvt(unsafe { libc::grantpt(master.as_raw_fd()) })?;
    sys::cvt(unsafe { libc::unlockpt(master.as_raw_fd()) })?;
    const TIOCGPTPEER: libc::c_ulong = 0x5441;
    let slave = sys::cvt(unsafe { libc::ioctl(master.as_raw_fd(), TIOCGPTPEER as _, flags) })?;
    Ok((master, unsafe { OwnedFd::from_raw_fd(slave) }))
}

fn set_size(fd: i32, rows: u16, cols: u16) {
    if rows == 0 || cols == 0 {
        return;
    }
    let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) };
}

fn run_tty(spec: ExecSpec, mut cmd: Command, stream: UnixStream, frames: Frames, reaper: Reaper) -> anyhow::Result<()> {
    let (master, slave) = open_pty()?;
    set_size(master.as_raw_fd(), spec.rows, spec.cols);
    let s = File::from(slave);
    cmd.stdin(s.try_clone()?).stdout(s.try_clone()?).stderr(s.try_clone()?);
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            libc::ioctl(0, libc::TIOCSCTTY, 0);
            Ok(())
        });
    }
    let (child, rx) = reaper.spawn(&mut cmd)?;
    let pid = child.id() as i32;
    drop(cmd);
    drop(s);
    frames.send(frames::STARTED, &(pid as u32).to_be_bytes())?;

    let master = File::from(master);
    let (done_tx, done_rx) = mpsc::channel();
    pump_out(Box::new(master.try_clone()?), frames::STDOUT, frames.clone(), done_tx);
    let mut input = stream;
    let mut to_pty = master;
    std::thread::spawn(move || {
        loop {
            match frames::read_frame(&mut input) {
                Ok(Some((frames::STDIN, data))) => {
                    let _ = to_pty.write_all(&data);
                }
                Ok(Some((frames::RESIZE, d))) if d.len() == 4 => {
                    set_size(to_pty.as_raw_fd(), u16::from_be_bytes([d[0], d[1]]), u16::from_be_bytes([d[2], d[3]]));
                }
                Ok(Some(_)) => {}
                _ => {
                    unsafe { libc::kill(-pid, libc::SIGHUP) };
                    break;
                }
            }
        }
    });
    exit_after(&frames, rx, done_rx, 1)
}
