//! berthd: makes, runs and removes berths, and serves the API on a Unix socket.

mod activity;
mod berth;
mod cgroup;
mod layer;
mod lifecycle;
mod paths;
mod proxy;
mod repo;
mod routes;
mod spawn;
mod store;

use std::collections::HashMap;
use std::fs;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{PermissionsExt, chown};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, anyhow, bail};
use serde_json::{Value, json};

use crate::api::{self, ExecSpec, http};
use crate::{image, net, pipe};
use berth::{Berth, CreateSpec, State};
use routes::Route;

pub use spawn::hold;

/// Each berth's user namespace maps its ids 0..65536 to a host range of its own,
/// starting here.
const UID_START: u32 = 1_000_000;
const UID_COUNT: u32 = 65536;
/// The berth user agents run as.
const AGENT_UID: u32 = 1000;

/// The berths this daemon knows, by id.
pub struct Daemon {
    berths: Mutex<HashMap<String, Berth>>,
    /// One lock per berth, held by anything that changes it.
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    activity: activity::Table,
}

pub fn main() -> anyhow::Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("berthd runs as root");
    }
    // Berths' inits enter berths/<id> as their own root. Images, layers and
    // repos are only ever read or mounted by the daemon (they hold setuid
    // files and private source): no one else may reach them.
    for (d, mode) in [
        (PathBuf::from(paths::STATE), 0o711),
        (paths::state("berths"), 0o711),
        (paths::state("images"), 0o700),
        (paths::state("layers"), 0o700),
        (paths::state("repos"), 0o700),
        (PathBuf::from(paths::RUN), 0o755),
        (paths::run("berths"), 0o700),
    ] {
        fs::create_dir_all(&d)?;
        fs::set_permissions(&d, fs::Permissions::from_mode(mode))?;
    }
    // The repos directory is owned by the fetch id, so fetches (which run as
    // it, not root) can write mirrors while it stays closed to other users.
    chown(paths::state("repos"), Some(repo::FETCH_UID), Some(repo::FETCH_UID))?;

    fs::create_dir_all(paths::CGROUP)?;
    let _ = fs::write("/sys/fs/cgroup/cgroup.subtree_control", "+cpu +memory +pids");
    fs::write(PathBuf::from(paths::CGROUP).join("cgroup.subtree_control"), "+cpu +memory +pids")
        .context("enabling cgroup controllers for berths")?;
    net::setup_host().context("setting up the berths' network")?;
    if fs::read_to_string("/proc/swaps").map(|s| s.lines().count() <= 1).unwrap_or(true) {
        eprintln!("berthd: this machine has no swap: idle berths stay frozen but keep their memory");
    }

    let d = Arc::new(Daemon { berths: Mutex::new(load_berths()), locks: Mutex::default(), activity: Mutex::default() });
    d.reconcile();
    {
        let berths = d.berths.lock().unwrap();
        let count = |s| berths.values().filter(|b| b.state == s).count();
        eprintln!(
            "berthd: {} berths: {} running, {} idle, {} stopped",
            berths.len(),
            count(State::Running),
            count(State::Idle),
            count(State::Stopped)
        );
    }
    let sweeper = d.clone();
    std::thread::spawn(move || sweeper.sweep());

    let socket = api::socket_path();
    if let Some(parent) = socket.parent() {
        fs::create_dir_all(parent)?;
    }
    let _ = fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).with_context(|| format!("listening on {}", socket.display()))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o660))?;
    if let Some(gid) = group_id("berth") {
        chown(&socket, Some(0), Some(gid))?;
    }
    eprintln!("berthd: listening on {}", socket.display());
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let d = d.clone();
        std::thread::spawn(move || d.serve(conn));
    }
    Ok(())
}

/// The berths recorded on disk, running or not.
fn load_berths() -> HashMap<String, Berth> {
    let mut berths = HashMap::new();
    for e in fs::read_dir(paths::state("berths")).into_iter().flatten().flatten() {
        if let Ok(data) = fs::read(e.path().join("state.json"))
            && let Ok(b) = serde_json::from_slice::<Berth>(&data)
        {
            berths.insert(b.id.clone(), b);
        }
    }
    berths
}

fn group_id(name: &str) -> Option<u32> {
    let c = std::ffi::CString::new(name).ok()?;
    let g = unsafe { libc::getgrnam(c.as_ptr()) };
    (!g.is_null()).then(|| unsafe { (*g).gr_gid })
}

/// Whether name is fit for a project or layer: one path component, not hidden.
fn valid_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('/') && !name.starts_with('.') && !name.contains('\0')
}

impl Daemon {
    /// Brings records in line with what's really there: berths whose init is
    /// gone (the machine rebooted, say) are stopped, and start again when used.
    fn reconcile(&self) {
        let ids: Vec<String> = self.berths.lock().unwrap().keys().cloned().collect();
        for id in ids {
            let Ok(mut b) = self.get(&id) else { continue };
            let state = match b.state {
                State::Running | State::Idle if !b.alive() => {
                    let _ = self.teardown(&b);
                    State::Stopped
                }
                State::Running | State::Idle if cgroup::frozen(&b) => State::Idle,
                State::Running | State::Idle => State::Running,
                State::Stopped => State::Stopped,
            };
            if state != b.state {
                b.state = state;
                let _ = self.put(&b);
            }
        }
    }

    fn lock_of(&self, id: &str) -> Arc<Mutex<()>> {
        self.locks.lock().unwrap().entry(id.to_string()).or_default().clone()
    }

    /// Records b, in memory and on disk.
    fn put(&self, b: &Berth) -> anyhow::Result<()> {
        b.save()?;
        self.berths.lock().unwrap().insert(b.id.clone(), b.clone());
        Ok(())
    }

    fn serve(self: Arc<Self>, mut conn: UnixStream) {
        let Ok(req) = http::read_request(&mut conn) else { return };
        let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
        let result = match Route::parse(&req.method, &req.path, req.upgrade) {
            Some(route) => self.handle(route, body, &mut conn),
            None => Err(anyhow!("no such endpoint: {} {}", req.method, req.path)),
        };
        match result {
            Ok(Some(v)) => {
                let _ = http::respond(&mut conn, 200, &v);
            }
            // The connection became a stream.
            Ok(None) => {}
            Err(e) => {
                let status = if e.to_string().starts_with("no berth") { 404 } else { 400 };
                let _ = http::respond(&mut conn, status, &json!({ "error": format!("{e:#}") }));
            }
        }
    }

    /// Answers a request with JSON, or with None once conn became a stream.
    fn handle(self: &Arc<Self>, route: Route, body: Value, conn: &mut UnixStream) -> anyhow::Result<Option<Value>> {
        Ok(Some(match route {
            Route::List => {
                let mut list: Vec<Value> = self.berths.lock().unwrap().values().map(Berth::view).collect();
                list.sort_by_key(|b| b["created"].as_u64());
                json!(list)
            }
            Route::Create => {
                let spec: CreateSpec = serde_json::from_value(body)?;
                self.create(spec)?.view()
            }
            Route::Get(id) => self.get(id)?.view(),
            Route::Destroy(id) => {
                self.destroy(id)?;
                json!({ "id": id })
            }
            Route::Ports(id) => {
                let b = self.get(id)?;
                let ports = if b.state != State::Stopped && b.alive() { net::listening_ports(b.pid) } else { vec![] };
                json!({ "ports": ports })
            }
            Route::Exec(id) => {
                let spec: ExecSpec = serde_json::from_value(body)?;
                if spec.service.is_some() {
                    let pid = self.add_service(id, spec)?;
                    http::respond_upgrade(conn)?;
                    api::frames::write_frame(conn, api::frames::STARTED, &pid.to_be_bytes())?;
                    return Ok(None);
                }
                // In use from before it wakes, so it can't be idled under us.
                let busy = self.stream(id);
                let b = self.wake(id)?;
                http::respond_upgrade(conn)?;
                spawn::hand_to_init(&b, &spec, conn.as_raw_fd())?;
                if !spec.detach {
                    self.stream_until_hangup(id, OwnedFd::from(conn.try_clone()?));
                }
                drop(busy);
                return Ok(None);
            }
            Route::Proxy(id) => {
                let port = body["port"].as_u64().and_then(|p| u16::try_from(p).ok()).context("port missing")?;
                let _busy = self.stream(id);
                let b = self.wake(id)?;
                let tcp = proxy::dial(b.pid, port)?;
                http::respond_upgrade(conn)?;
                pipe::splice(conn.try_clone()?, tcp)?;
                return Ok(None);
            }
            Route::Start(id) => self.wake(id)?.view(),
            Route::Idle(id) => self.idle(id, None)?.view(),
            Route::Stop(id) => self.stop(id, None)?.view(),
            Route::Services(id) => json!(self.get(id)?.services),
            Route::RemoveService(id, name) => {
                self.remove_service(id, name)?;
                json!({ "service": name })
            }
            Route::PullImage => {
                let reference = body["reference"].as_str().context("reference missing")?;
                let root = image::pull(&paths::state("images"), reference)?;
                json!({ "reference": reference, "rootfs": root })
            }
            Route::Images => json!(store::list(&paths::state("images"), |p| p.join("rootfs").is_dir())),
            Route::Repos => json!(
                store::list(&paths::state("repos"), |_| true)
                    .into_iter()
                    .filter_map(|n| n.strip_suffix(".git").map(String::from))
                    .collect::<Vec<_>>()
            ),
            Route::Layers => json!(store::list(&paths::state("layers"), |_| true)),
            Route::RemoveRepo(name) => {
                self.unused("project", name, |b| b.project.as_deref() == Some(name))?;
                store::remove(&paths::state("repos"), &format!("{name}.git"))?;
                json!({ "name": name })
            }
            Route::RemoveLayer(name) => {
                self.unused("layer", name, |b| b.layers.iter().any(|l| l == name))?;
                store::remove(&paths::state("layers"), name)?;
                json!({ "name": name })
            }
            Route::AddRepo => {
                let name = body["name"].as_str().context("name missing")?;
                let url = body["url"].as_str().context("url missing")?;
                repo::add_repo(name, url)?;
                json!({ "name": name })
            }
            Route::CommitLayer => {
                let id = body["berth"].as_str().context("berth missing")?;
                let name = body["name"].as_str().context("name missing")?;
                self.commit_layer(id, name)?;
                json!({ "name": name })
            }
        }))
    }

    /// Fails if a berth uses what's about to be removed.
    fn unused(&self, what: &str, name: &str, uses: impl Fn(&Berth) -> bool) -> anyhow::Result<()> {
        let users: Vec<String> =
            self.berths.lock().unwrap().values().filter(|b| uses(b)).map(|b| b.id.clone()).collect();
        if !users.is_empty() {
            bail!("{what} {name} is used by berths {}", users.join(", "));
        }
        Ok(())
    }

    fn get(&self, id: &str) -> anyhow::Result<Berth> {
        self.berths.lock().unwrap().get(id).cloned().with_context(|| format!("no berth {id}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_single_visible_components() {
        for good in ["demo", "tools", "my-project_2", "a.b"] {
            assert!(valid_name(good), "{good}");
        }
        for bad in ["", ".", "..", ".hidden", "a/b", "../x", "a\0b"] {
            assert!(!valid_name(bad), "{bad:?}");
        }
    }
}
