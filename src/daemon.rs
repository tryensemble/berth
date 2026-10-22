//! berthd: makes, runs and removes berths, and serves the API on a Unix socket.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{chown, lchown, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::init::{InitConfig, LISTEN_FD};
use crate::proto::{self, ExecSpec};
use crate::{image, net, sys};

const ROOT: &str = "/var/lib/berth";
const RUN: &str = "/run/berth";
const CGROUP: &str = "/sys/fs/cgroup/berth";
/// Each berth's user namespace maps its ids 0..65536 to a host range of its own.
const UID_START: u32 = 1_000_000;
const UID_COUNT: u32 = 65536;
/// The berth user agents run as.
const AGENT_UID: u32 = 1000;

fn dir(sub: &str) -> PathBuf {
    Path::new(ROOT).join(sub)
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Limits {
    pub cpus: f64,
    pub memory: String,
    pub pids: u32,
    pub tmp: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CreateSpec {
    pub project: Option<String>,
    pub branch: Option<String>,
    #[serde(default = "default_image")]
    pub image: String,
    #[serde(default)]
    pub layers: Vec<String>,
    #[serde(default = "default_cpus")]
    pub cpus: f64,
    #[serde(default = "default_memory")]
    pub memory: String,
    #[serde(default = "default_pids")]
    pub pids: u32,
    #[serde(default = "default_tmp")]
    pub tmp: String,
}

fn default_image() -> String {
    "ubuntu:24.04".into()
}
fn default_cpus() -> f64 {
    2.0
}
fn default_memory() -> String {
    "4G".into()
}
fn default_pids() -> u32 {
    4096
}
fn default_tmp() -> String {
    "1G".into()
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Berth {
    pub id: String,
    pub n: u32,
    pub uid_base: u32,
    pub ip: String,
    /// The berth's PID 1, as the host sees it.
    pub pid: i32,
    pub project: Option<String>,
    pub branch: Option<String>,
    pub image: String,
    pub layers: Vec<String>,
    pub limits: Limits,
    pub created: u64,
}

impl Berth {
    fn dir(&self) -> PathBuf {
        dir("berths").join(&self.id)
    }
    fn run_dir(&self) -> PathBuf {
        Path::new(RUN).join("berths").join(&self.id)
    }
    fn cgroup(&self) -> PathBuf {
        Path::new(CGROUP).join(&self.id)
    }
    /// Whether its PID 1 still runs, and is still ours (pids get reused).
    fn alive(&self) -> bool {
        sys::pid_alive(self.pid)
            && fs::read_to_string(format!("/proc/{}/cgroup", self.pid))
                .map(|c| c.contains(&format!("/berth/{}", self.id)))
                .unwrap_or(false)
    }
    fn save(&self) -> anyhow::Result<()> {
        fs::write(self.dir().join("state.json"), serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
    fn view(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap();
        v["status"] = json!(if self.alive() { "running" } else { "dead" });
        v
    }
}

struct Daemon {
    berths: Mutex<HashMap<String, Berth>>,
}

pub fn main() -> anyhow::Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("berthd runs as root");
    }
    for (d, mode) in [
        (ROOT.into(), 0o711),
        (dir("berths"), 0o711),
        (dir("images"), 0o700),
        (dir("layers"), 0o711),
        (dir("repos"), 0o711),
        (PathBuf::from(RUN), 0o755),
        (Path::new(RUN).join("berths"), 0o700),
    ] {
        fs::create_dir_all(&d)?;
        fs::set_permissions(&d, fs::Permissions::from_mode(mode))?;
    }
    // The images and layers are shared by every berth: readable to their
    // namespaces through idmapped mounts, so the directories above them need x.
    fs::set_permissions(dir("images"), fs::Permissions::from_mode(0o711))?;

    fs::create_dir_all(CGROUP)?;
    let _ = fs::write("/sys/fs/cgroup/cgroup.subtree_control", "+cpu +memory +pids");
    fs::write(Path::new(CGROUP).join("cgroup.subtree_control"), "+cpu +memory +pids")
        .context("enabling cgroup controllers for berths")?;
    net::setup_host().context("setting up the berths' network")?;

    let mut berths = HashMap::new();
    if let Ok(rd) = fs::read_dir(dir("berths")) {
        for e in rd.flatten() {
            if let Ok(data) = fs::read(e.path().join("state.json")) {
                if let Ok(b) = serde_json::from_slice::<Berth>(&data) {
                    berths.insert(b.id.clone(), b);
                }
            }
        }
    }
    let alive = berths.values().filter(|b| b.alive()).count();
    eprintln!("berthd: {} berths, {alive} running", berths.len());
    let d = Arc::new(Daemon { berths: Mutex::new(berths) });

    let _ = fs::remove_file(proto::SOCKET);
    let listener = UnixListener::bind(proto::SOCKET)?;
    fs::set_permissions(proto::SOCKET, fs::Permissions::from_mode(0o660))?;
    if let Some(gid) = group_id("berth") {
        chown(proto::SOCKET, Some(0), Some(gid))?;
    }
    eprintln!("berthd: listening on {}", proto::SOCKET);
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let d = d.clone();
        std::thread::spawn(move || d.serve(conn));
    }
    Ok(())
}

fn group_id(name: &str) -> Option<u32> {
    let c = std::ffi::CString::new(name).ok()?;
    let g = unsafe { libc::getgrnam(c.as_ptr()) };
    (!g.is_null()).then(|| unsafe { (*g).gr_gid })
}

fn parse_size(s: &str) -> anyhow::Result<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('K' | 'k') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M' | 'm') => (&s[..s.len() - 1], 1 << 20),
        Some('G' | 'g') => (&s[..s.len() - 1], 1 << 30),
        _ => (s, 1),
    };
    Ok((num.parse::<f64>().with_context(|| format!("bad size {s}"))? * mult as f64) as u64)
}

fn random_id() -> String {
    let mut b = [0u8; 4];
    File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).expect("urandom");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Daemon {
    fn serve(&self, mut conn: UnixStream) {
        let req = match proto::read_request(&mut conn) {
            Ok(r) => r,
            Err(_) => return,
        };
        let parts: Vec<&str> = req.path.trim_matches('/').split('/').collect();
        let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
        let result: anyhow::Result<Option<Value>> = (|| {
            Ok(Some(match (req.method.as_str(), parts.as_slice()) {
                ("GET", ["v1", "berths"]) => {
                    let mut list: Vec<Value> = self.berths.lock().unwrap().values().map(Berth::view).collect();
                    list.sort_by_key(|b| b["created"].as_u64());
                    json!(list)
                }
                ("POST", ["v1", "berths"]) => {
                    let spec: CreateSpec = serde_json::from_value(body)?;
                    self.create(spec)?.view()
                }
                ("GET", ["v1", "berths", id]) => self.get(id)?.view(),
                ("DELETE", ["v1", "berths", id]) => {
                    self.destroy(id)?;
                    json!({ "id": id })
                }
                ("GET", ["v1", "berths", id, "ports"]) => {
                    let b = self.running(id)?;
                    json!({ "ports": net::listening_ports(b.pid) })
                }
                ("POST", ["v1", "berths", id, "exec"]) if req.upgrade => {
                    let spec: ExecSpec = serde_json::from_value(body)?;
                    let b = self.running(id)?;
                    proto::respond_upgrade(&mut conn)?;
                    hand_to_init(&b, &spec, conn.as_raw_fd())?;
                    return Ok(None);
                }
                ("POST", ["v1", "berths", id, "proxy"]) if req.upgrade => {
                    let port = body["port"].as_u64().context("port missing")? as u16;
                    let b = self.running(id)?;
                    let tcp = dial(b.pid, port)?;
                    proto::respond_upgrade(&mut conn)?;
                    splice(conn.try_clone()?, tcp);
                    return Ok(None);
                }
                ("POST", ["v1", "images"]) => {
                    let reference = body["reference"].as_str().context("reference missing")?;
                    let root = image::pull(&dir("images"), reference)?;
                    json!({ "reference": reference, "rootfs": root })
                }
                ("POST", ["v1", "repos"]) => {
                    let name = body["name"].as_str().context("name missing")?;
                    let url = body["url"].as_str().context("url missing")?;
                    add_repo(name, url)?;
                    json!({ "name": name })
                }
                ("POST", ["v1", "layers"]) => {
                    let id = body["berth"].as_str().context("berth missing")?;
                    let name = body["name"].as_str().context("name missing")?;
                    self.commit_layer(id, name)?;
                    json!({ "name": name })
                }
                _ => return Err(anyhow!("no such endpoint: {} {}", req.method, req.path)),
            }))
        })();
        match result {
            Ok(Some(v)) => {
                let _ = proto::respond(&mut conn, 200, &v);
            }
            Ok(None) => {}
            Err(e) => {
                let status = if format!("{e}").starts_with("no berth") { 404 } else { 400 };
                let _ = proto::respond(&mut conn, status, &json!({ "error": format!("{e:#}") }));
            }
        }
    }

    fn get(&self, id: &str) -> anyhow::Result<Berth> {
        self.berths.lock().unwrap().get(id).cloned().with_context(|| format!("no berth {id}"))
    }

    fn running(&self, id: &str) -> anyhow::Result<Berth> {
        let b = self.get(id)?;
        if !b.alive() {
            bail!("berth {id} isn't running");
        }
        Ok(b)
    }

    fn create(&self, spec: CreateSpec) -> anyhow::Result<Berth> {
        let image_root = image::rootfs(&dir("images"), &spec.image);
        if !image_root.exists() {
            image::pull(&dir("images"), &spec.image)?;
        }
        let mut lowers = vec![image_root];
        for l in &spec.layers {
            let p = dir("layers").join(l);
            if !p.exists() {
                bail!("no layer {l}; build it with `berth layer`");
            }
            lowers.push(p);
        }
        let mirror = match &spec.project {
            Some(p) => {
                let m = dir("repos").join(format!("{p}.git"));
                if !m.exists() {
                    bail!("no project {p}; add it with `berth repo`");
                }
                Some(m)
            }
            None => None,
        };

        // Reserve a number (hence ids and an address) under the lock.
        let b = {
            let mut berths = self.berths.lock().unwrap();
            let used: Vec<u32> = berths.values().map(|b| b.n).collect();
            let n = (2..).find(|n| !used.contains(n)).unwrap();
            let b = Berth {
                id: random_id(),
                n,
                uid_base: UID_START + n * UID_COUNT,
                ip: net::address(n).to_string(),
                pid: 0,
                project: spec.project.clone(),
                branch: spec.branch.clone(),
                image: spec.image.clone(),
                layers: spec.layers.clone(),
                limits: Limits { cpus: spec.cpus, memory: spec.memory.clone(), pids: spec.pids, tmp: spec.tmp.clone() },
                created: now(),
            };
            berths.insert(b.id.clone(), b.clone());
            b
        };
        match self.build(b.clone(), &lowers, mirror) {
            Ok(b) => {
                self.berths.lock().unwrap().insert(b.id.clone(), b.clone());
                Ok(b)
            }
            Err(e) => {
                let _ = self.destroy(&b.id);
                Err(e)
            }
        }
    }

    fn build(&self, mut b: Berth, lowers: &[PathBuf], mirror: Option<PathBuf>) -> anyhow::Result<Berth> {
        let d = b.dir();
        let base = b.uid_base;
        for sub in ["upper", "work", "root", "workspace"] {
            fs::create_dir_all(d.join(sub))?;
        }
        fs::set_permissions(&d, fs::Permissions::from_mode(0o711))?;
        chown(d.join("upper"), Some(base), Some(base))?;
        chown(d.join("workspace"), Some(base + AGENT_UID), Some(base + AGENT_UID))?;
        b.save()?;

        // The berth's filesystem: the image and layers, shifted into its id
        // range through idmapped mounts, under an overlay with its own upper.
        let userns = new_userns(base)?;
        let mut targets = Vec::new();
        for (i, lower) in lowers.iter().enumerate() {
            let t = d.join(format!("lower{i}"));
            fs::create_dir_all(&t)?;
            sys::idmapped_bind(lower, &t, userns.as_raw_fd())
                .with_context(|| format!("idmapped mount of {}", lower.display()))?;
            targets.push(t);
        }
        let lowerdir: Vec<String> = targets.iter().rev().map(|t| t.display().to_string()).collect();
        let data = format!(
            "lowerdir={},upperdir={},workdir={}",
            lowerdir.join(":"),
            d.join("upper").display(),
            d.join("work").display()
        );
        let mounted = sys::mount(Some("overlay"), &d.join("root"), Some("overlay"), 0, Some(&data));
        // The overlay keeps its own references to its layers.
        for t in &targets {
            let _ = sys::umount(t, libc::MNT_DETACH);
            let _ = fs::remove_dir(t);
        }
        mounted.context("mounting the berth's root")?;

        let cg = b.cgroup();
        fs::create_dir_all(&cg)?;
        let mem = parse_size(&b.limits.memory)?;
        fs::write(cg.join("cpu.max"), format!("{} 100000", (b.limits.cpus * 100_000.0) as u64))?;
        fs::write(cg.join("memory.max"), mem.to_string())?;
        fs::write(cg.join("memory.high"), (mem / 4 * 3).to_string())?;
        fs::write(cg.join("pids.max"), b.limits.pids.to_string())?;

        let rd = b.run_dir();
        fs::create_dir_all(&rd)?;
        let sock = rd.join("init.sock");
        let _ = fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock)?;
        fs::set_permissions(&sock, fs::Permissions::from_mode(0o600))?;

        let cfg = InitConfig {
            id: b.id.clone(),
            hostname: format!("berth-{}", b.id),
            root: d.join("root"),
            workspace: d.join("workspace"),
            mirror: mirror.clone(),
            dns: net::resolvers(),
            tmp_size: b.limits.tmp.clone(),
        };
        let (pid, mut go, mut out) = spawn_init(&b, &cfg, &userns, &listener)?;
        drop(listener);
        b.pid = pid;
        b.save()?;

        net::attach(b.n, pid).context("attaching the berth's network")?;
        go.write_all(b"go\n")?;
        let mut line = String::new();
        out.read_line(&mut line)?;
        if line.trim() != "ready" {
            let log = fs::read_to_string(d.join("init.log")).unwrap_or_default();
            bail!("the berth didn't start: {}", log.trim());
        }

        if let (Some(mirror), Some(project)) = (&mirror, &b.project) {
            let url = sys::run("git", &["--git-dir", &mirror.display().to_string(), "config", "--get", "remote.origin.url"])
                .unwrap_or_default();
            let script = "set -e
git -c safe.directory='*' clone -q --shared /berth/repo.git /workspace
cd /workspace
if [ -n \"$1\" ]; then git checkout -q \"$1\" 2>/dev/null || git checkout -q -b \"$1\"; fi
if [ -n \"$2\" ]; then git remote set-url origin \"$2\"; fi";
            let spec = ExecSpec {
                argv: vec!["sh".into(), "-c".into(), script.into(), "sh".into(), b.branch.clone().unwrap_or_default(), url.trim().into()],
                ..Default::default()
            };
            let (code, output) = exec_collect(&b, &spec)?;
            if code != 0 {
                bail!("checking out {project} failed: {}", output.trim());
            }
        }
        Ok(b)
    }

    fn stop(&self, b: &Berth) {
        let cg = b.cgroup();
        if cg.exists() {
            let _ = fs::write(cg.join("cgroup.kill"), "1");
            for _ in 0..50 {
                let events = fs::read_to_string(cg.join("cgroup.events")).unwrap_or_default();
                if events.contains("populated 0") {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        net::detach(b.n);
        let _ = sys::umount(&b.dir().join("root"), libc::MNT_DETACH);
    }

    fn destroy(&self, id: &str) -> anyhow::Result<()> {
        let b = self.get(id)?;
        self.stop(&b);
        let _ = fs::remove_dir_all(b.run_dir());
        let _ = fs::remove_dir(b.cgroup());
        if b.dir().join("root").read_dir().map(|mut r| r.next().is_some()).unwrap_or(false) {
            bail!("berth {id}'s root is still mounted; not removing its files");
        }
        fs::remove_dir_all(b.dir()).or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) })?;
        self.berths.lock().unwrap().remove(id);
        Ok(())
    }

    /// Keeps what a berth installed (its upper layer) as a read-only layer
    /// other berths can be made from, and removes the berth.
    fn commit_layer(&self, id: &str, name: &str) -> anyhow::Result<()> {
        if name.is_empty() || name.contains('/') || name.starts_with('.') {
            bail!("bad layer name {name}");
        }
        let b = self.get(id)?;
        self.stop(&b);
        let upper = b.dir().join("upper");
        for p in ["workspace", "berth", "var/log/berth", "etc/resolv.conf", "etc/hostname", "etc/hosts"] {
            let p = upper.join(p);
            let _ = if p.is_dir() { fs::remove_dir_all(&p) } else { fs::remove_file(&p) };
        }
        shift_owners(&upper, b.uid_base)?;
        let target = dir("layers").join(name);
        if target.exists() {
            fs::remove_dir_all(&target)?;
        }
        fs::rename(&upper, &target)?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755))?;
        self.destroy(id)
    }
}

/// Moves files a berth owned (ids base..base+65536) back to ids from 0, so the
/// layer can be shifted into any other berth's range.
fn shift_owners(p: &Path, base: u32) -> anyhow::Result<()> {
    let m = fs::symlink_metadata(p)?;
    let shift = |id: u32| if (base..base + UID_COUNT).contains(&id) { id - base } else { id };
    let (uid, gid) = (shift(m.uid()), shift(m.gid()));
    if (uid, gid) != (m.uid(), m.gid()) {
        // chown clears setuid bits; put the mode back.
        lchown(p, Some(uid), Some(gid))?;
        if !m.file_type().is_symlink() {
            fs::set_permissions(p, fs::Permissions::from_mode(m.mode() & 0o7777))?;
        }
    }
    if m.is_dir() {
        for e in fs::read_dir(p)? {
            shift_owners(&e?.path(), base)?;
        }
    }
    Ok(())
}

fn add_repo(name: &str, url: &str) -> anyhow::Result<()> {
    if name.is_empty() || name.contains('/') || name.starts_with('.') {
        bail!("bad project name {name}");
    }
    let path = dir("repos").join(format!("{name}.git"));
    let p = path.display().to_string();
    if path.exists() {
        sys::run("git", &["--git-dir", &p, "remote", "set-url", "origin", url])?;
        sys::run("git", &["--git-dir", &p, "-c", "remote.origin.uploadpack=git -c safe.directory='*' upload-pack", "remote", "update", "--prune"])?;
    } else {
        // A local path may belong to another user; the mirror only reads it.
        sys::run("git", &["-c", "safe.directory=*", "clone", "-q", "--mirror", "--upload-pack", "git -c safe.directory='*' upload-pack", url, &p])?;
    }
    Ok(())
}

/// A user namespace mapping ids 0..65536 to base.., kept by its fd.
fn new_userns(base: u32) -> anyhow::Result<OwnedFd> {
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
fn spawn_init(
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
fn hand_to_init(b: &Berth, spec: &ExecSpec, stream_fd: i32) -> anyhow::Result<()> {
    let control = UnixStream::connect(b.run_dir().join("init.sock")).context("reaching the berth's init")?;
    sys::send_with_fd(&control, &serde_json::to_vec(spec)?, stream_fd)?;
    Ok(())
}

/// Runs a command in a berth and collects its exit code and output.
fn exec_collect(b: &Berth, spec: &ExecSpec) -> anyhow::Result<(i32, String)> {
    let (mut mine, theirs) = UnixStream::pair()?;
    hand_to_init(b, spec, theirs.as_raw_fd())?;
    drop(theirs);
    let mut output = Vec::new();
    while let Some((kind, data)) = proto::read_frame(&mut mine)? {
        match kind {
            proto::STDOUT | proto::STDERR | proto::ERROR => output.extend_from_slice(&data),
            proto::EXIT if data.len() == 4 => {
                return Ok((i32::from_be_bytes(data.try_into().unwrap()), String::from_utf8_lossy(&output).into()));
            }
            _ => {}
        }
    }
    bail!("the berth's init hung up: {}", String::from_utf8_lossy(&output))
}

/// Connects to port on the berth's own loopback, from inside its network
/// namespace, so servers bound to 127.0.0.1 are reachable too.
fn dial(pid: i32, port: u16) -> anyhow::Result<TcpStream> {
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

/// Copies both ways until both sides are done.
fn splice(unix: UnixStream, tcp: TcpStream) {
    let (mut u_in, mut t_out) = (unix.try_clone().unwrap(), tcp.try_clone().unwrap());
    let up = std::thread::spawn(move || {
        let _ = std::io::copy(&mut u_in, &mut t_out);
        let _ = t_out.shutdown(Shutdown::Write);
    });
    let (mut t_in, mut u_out) = (tcp, unix);
    let _ = std::io::copy(&mut t_in, &mut u_out);
    let _ = u_out.shutdown(Shutdown::Write);
    let _ = up.join();
}

pub fn hold() {
    let mut buf = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut buf);
}
