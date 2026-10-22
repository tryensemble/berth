//! berth-init: the first process of a berth. The daemon starts it already in
//! the berth's user namespace and with fresh mount, PID, network, IPC, UTS and
//! cgroup namespaces; it forks the berth's PID 1, which builds the berth's
//! filesystem, pivots into it, and from then on starts every command run in the
//! berth. Commands are its children, not the daemon's, so they outlive the
//! daemon.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{symlink, OpenOptionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};

use crate::proto::{self, ExecSpec, Frames};
use crate::sys;

pub const LISTEN_FD: i32 = 3;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct InitConfig {
    pub id: String,
    pub hostname: String,
    /// The berth's root filesystem, an overlay the daemon mounted.
    pub root: PathBuf,
    pub workspace: PathBuf,
    /// The project's bare mirror, mounted read-only at /berth/repo.git.
    pub mirror: Option<PathBuf>,
    pub dns: Vec<String>,
    pub tmp_size: String,
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
    build_root(&cfg).context("building the berth's filesystem")?;
    let host = std::ffi::CString::new(cfg.hostname.clone())?;
    sys::cvt(unsafe { libc::sethostname(host.as_ptr(), cfg.hostname.len()) })?;

    println!("ready");
    std::io::stdout().flush()?;
    let null = File::options().read(true).write(true).open("/dev/null")?;
    unsafe {
        libc::dup2(null.as_raw_fd(), 0);
        libc::dup2(null.as_raw_fd(), 1);
    }

    let reaper = Reaper::start();
    let listener = unsafe { UnixListener::from_raw_fd(LISTEN_FD) };
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let reaper = reaper.clone();
        std::thread::spawn(move || {
            if let Err(e) = handle(conn, reaper) {
                eprintln!("berth-init: {e:#}");
            }
        });
    }
    Ok(())
}

fn build_root(cfg: &InitConfig) -> anyhow::Result<()> {
    use libc::{MS_BIND, MS_NODEV, MS_NOEXEC, MS_NOSUID, MS_PRIVATE, MS_RDONLY, MS_REC, MS_REMOUNT};
    let root = &cfg.root;
    sys::mount(None, Path::new("/"), None, MS_REC | MS_PRIVATE, None)?;
    sys::mount(Some(path_str(root)), root, None, MS_BIND | MS_REC, None)?;
    let at = |p: &str| root.join(p.trim_start_matches('/'));
    for d in ["proc", "sys", "dev", "tmp", "workspace", "berth", ".oldroot", "var/log/berth"] {
        fs::create_dir_all(at(d))?;
    }

    sys::mount(Some("proc"), &at("proc"), Some("proc"), MS_NOSUID | MS_NODEV | MS_NOEXEC, None)?;
    if let Err(e) = sys::mount(Some("sysfs"), &at("sys"), Some("sysfs"), MS_RDONLY | MS_NOSUID | MS_NODEV | MS_NOEXEC, None) {
        eprintln!("berth-init: no /sys: {e}");
    }

    let dev = at("dev");
    sys::mount(Some("tmpfs"), &dev, Some("tmpfs"), MS_NOSUID | MS_NOEXEC, Some("mode=755,size=64k"))?;
    for name in ["null", "zero", "full", "random", "urandom", "tty"] {
        let target = dev.join(name);
        File::create(&target)?;
        sys::mount(Some(&format!("/dev/{name}")), &target, None, MS_BIND, None)?;
    }
    fs::create_dir_all(dev.join("pts"))?;
    sys::mount(
        Some("devpts"),
        &dev.join("pts"),
        Some("devpts"),
        MS_NOSUID | MS_NOEXEC,
        Some("newinstance,ptmxmode=0666,mode=0620,gid=5"),
    )?;
    symlink("pts/ptmx", dev.join("ptmx"))?;
    fs::create_dir_all(dev.join("shm"))?;
    sys::mount(Some("tmpfs"), &dev.join("shm"), Some("tmpfs"), MS_NOSUID | MS_NODEV, Some("mode=1777"))?;
    symlink("/proc/self/fd", dev.join("fd"))?;
    for (i, name) in ["stdin", "stdout", "stderr"].iter().enumerate() {
        symlink(format!("/proc/self/fd/{i}"), dev.join(name))?;
    }

    sys::mount(
        Some("tmpfs"),
        &at("tmp"),
        Some("tmpfs"),
        MS_NOSUID | MS_NODEV,
        Some(&format!("mode=1777,size={}", cfg.tmp_size)),
    )?;
    sys::mount(Some(path_str(&cfg.workspace)), &at("workspace"), None, MS_BIND, None)?;
    if let Some(mirror) = &cfg.mirror {
        let target = at("berth/repo.git");
        fs::create_dir_all(&target)?;
        sys::mount(Some(path_str(mirror)), &target, None, MS_BIND, None)?;
        sys::mount(None, &target, None, MS_BIND | MS_REMOUNT | MS_RDONLY, None)?;
    }

    let etc = at("etc");
    for f in ["resolv.conf", "hostname", "hosts"] {
        let _ = fs::remove_file(etc.join(f));
    }
    let resolv: String = cfg.dns.iter().map(|d| format!("nameserver {d}\n")).collect();
    fs::write(etc.join("resolv.conf"), resolv)?;
    fs::write(etc.join("hostname"), format!("{}\n", cfg.hostname))?;
    fs::write(
        etc.join("hosts"),
        format!("127.0.0.1 localhost\n::1 localhost\n127.0.1.1 {}\n", cfg.hostname),
    )?;

    let old = at(".oldroot");
    let (r, o) = (sys::cstr(root), sys::cstr(&old));
    sys::cvt(unsafe { libc::syscall(libc::SYS_pivot_root, r.as_ptr(), o.as_ptr()) as libc::c_int })
        .context("pivot_root")?;
    std::env::set_current_dir("/")?;
    sys::umount(Path::new("/.oldroot"), libc::MNT_DETACH)?;
    fs::remove_dir("/.oldroot")?;
    Ok(())
}

fn path_str(p: &Path) -> &str {
    p.to_str().expect("utf-8 path")
}

/// Reaps every child (PID 1 inherits orphans) and hands exit codes to whoever
/// waits for that pid.
#[derive(Clone)]
struct Reaper(Arc<Mutex<HashMap<i32, mpsc::Sender<i32>>>>);

impl Reaper {
    fn start() -> Self {
        let r = Reaper(Arc::new(Mutex::new(HashMap::new())));
        let map = r.0.clone();
        std::thread::spawn(move || loop {
            let mut status = 0;
            let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
            if pid < 0 {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            let code = if libc::WIFEXITED(status) {
                libc::WEXITSTATUS(status)
            } else {
                128 + libc::WTERMSIG(status)
            };
            if let Some(tx) = map.lock().unwrap().remove(&pid) {
                let _ = tx.send(code);
            }
        });
        r
    }

    /// Spawns cmd while holding the table, so its exit can't be reaped before
    /// it's registered.
    fn spawn(&self, cmd: &mut Command) -> std::io::Result<(Child, mpsc::Receiver<i32>)> {
        let mut map = self.0.lock().unwrap();
        let child = cmd.spawn()?;
        let (tx, rx) = mpsc::channel();
        map.insert(child.id() as i32, tx);
        Ok((child, rx))
    }
}

struct User {
    uid: u32,
    gid: u32,
    name: String,
    home: String,
    shell: String,
}

fn lookup_user(name: &str) -> Option<User> {
    let passwd = fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        if f.len() < 7 || (f[0] != name && f[2] != name) {
            return None;
        }
        Some(User {
            name: f[0].into(),
            uid: f[2].parse().ok()?,
            gid: f[3].parse().ok()?,
            home: f[5].into(),
            shell: f[6].into(),
        })
    })
}

fn handle(control: UnixStream, reaper: Reaper) -> anyhow::Result<()> {
    let (msg, fd) = sys::recv_with_fd(&control)?;
    drop(control);
    let spec: ExecSpec = serde_json::from_slice(&msg)?;
    let fd = fd.context("no stream attached")?;
    let stream = UnixStream::from(fd);
    let frames = Frames::new(stream.try_clone()?);
    if let Err(e) = run(spec, stream, frames.clone(), reaper) {
        let _ = frames.send(proto::ERROR, format!("{e:#}").as_bytes());
        let _ = frames.send(proto::EXIT, &127i32.to_be_bytes());
    }
    Ok(())
}

fn run(spec: ExecSpec, stream: UnixStream, frames: Frames, reaper: Reaper) -> anyhow::Result<()> {
    if spec.argv.is_empty() {
        bail!("no command");
    }
    let wanted = spec.user.clone().unwrap_or_else(|| {
        if lookup_user("agent").is_some() { "agent".into() } else { "root".into() }
    });
    let user = lookup_user(&wanted).with_context(|| format!("no user {wanted} in the berth"))?;

    let mut cmd = Command::new(&spec.argv[0]);
    cmd.args(&spec.argv[1..]).env_clear();
    let path = if user.uid == 0 {
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string()
    } else {
        format!("{}/.local/bin:/usr/local/bin:/usr/bin:/bin:/usr/local/sbin:/usr/sbin:/sbin", user.home)
    };
    cmd.env("PATH", path)
        .env("HOME", &user.home)
        .env("USER", &user.name)
        .env("LOGNAME", &user.name)
        .env("SHELL", &user.shell)
        .env("LANG", "C.UTF-8");
    if spec.tty {
        cmd.env("TERM", "xterm-256color");
    }
    for kv in &spec.env {
        if let Some((k, v)) = kv.split_once('=') {
            cmd.env(k, v);
        }
    }
    let cwd = spec.cwd.clone().unwrap_or_else(|| {
        if Path::new("/workspace/.git").exists() || user.uid != 0 { "/workspace".into() } else { "/".into() }
    });
    cmd.current_dir(&cwd).uid(user.uid).gid(user.gid);

    if spec.detach {
        let name = spec.name.clone().unwrap_or_else(|| "detached".into());
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o644)
            .open(format!("/var/log/berth/{}.log", name.replace('/', "_")))?;
        unsafe { libc::fchown(log.as_raw_fd(), user.uid, user.gid) };
        cmd.stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let (child, _) = reaper.spawn(&mut cmd)?;
        frames.send(proto::STARTED, &child.id().to_be_bytes())?;
        return Ok(());
    }

    if spec.tty {
        run_tty(spec, cmd, stream, frames, reaper)
    } else {
        run_pipes(cmd, stream, frames, reaper)
    }
}

fn exit_after(frames: &Frames, rx: mpsc::Receiver<i32>, outputs: mpsc::Receiver<()>, n: usize) -> anyhow::Result<()> {
    let code = rx.recv().unwrap_or(255);
    // Let the output drain; a background grandchild may hold it open forever.
    for _ in 0..n {
        if outputs.recv_timeout(Duration::from_millis(500)).is_err() {
            break;
        }
    }
    frames.send(proto::EXIT, &code.to_be_bytes())?;
    let _ = frames.0.lock().unwrap().shutdown(std::net::Shutdown::Both);
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
    frames.send(proto::STARTED, &(pid as u32).to_be_bytes())?;

    let (done_tx, done_rx) = mpsc::channel();
    for (out, kind) in [
        (Box::new(child.stdout.take().unwrap()) as Box<dyn Read + Send>, proto::STDOUT),
        (Box::new(child.stderr.take().unwrap()), proto::STDERR),
    ] {
        pump_out(out, kind, frames.clone(), done_tx.clone());
    }
    let mut stdin = child.stdin.take();
    let mut input = stream;
    std::thread::spawn(move || {
        loop {
            match proto::read_frame(&mut input) {
                Ok(Some((proto::STDIN, data))) => {
                    if let Some(s) = stdin.as_mut() {
                        let _ = s.write_all(&data);
                    }
                }
                Ok(Some((proto::EOF, _))) => stdin = None,
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

fn set_size(fd: i32, rows: u16, cols: u16) {
    if rows == 0 || cols == 0 {
        return;
    }
    let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) };
}

fn run_tty(spec: ExecSpec, mut cmd: Command, stream: UnixStream, frames: Frames, reaper: Reaper) -> anyhow::Result<()> {
    let (mut master, mut slave) = (0, 0);
    sys::cvt(unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null(), std::ptr::null()) })?;
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
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
    frames.send(proto::STARTED, &(pid as u32).to_be_bytes())?;

    let master = File::from(master);
    let (done_tx, done_rx) = mpsc::channel();
    pump_out(Box::new(master.try_clone()?), proto::STDOUT, frames.clone(), done_tx);
    let mut input = stream;
    let mut to_pty = master;
    std::thread::spawn(move || loop {
        match proto::read_frame(&mut input) {
            Ok(Some((proto::STDIN, data))) => {
                let _ = to_pty.write_all(&data);
            }
            Ok(Some((proto::RESIZE, d))) if d.len() == 4 => {
                set_size(to_pty.as_raw_fd(), u16::from_be_bytes([d[0], d[1]]), u16::from_be_bytes([d[2], d[3]]));
            }
            Ok(Some(_)) => {}
            _ => {
                unsafe { libc::kill(-pid, libc::SIGHUP) };
                break;
            }
        }
    });
    exit_after(&frames, rx, done_rx, 1)
}
