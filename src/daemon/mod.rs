//! berthd: makes, runs and removes berths, and serves the API on a Unix socket.

mod berth;
mod layer;
mod lifecycle;
mod paths;
mod proxy;
mod repo;
mod routes;
mod spawn;

use std::collections::HashMap;
use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{PermissionsExt, chown};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, anyhow, bail};
use serde_json::{Value, json};

use crate::api::{self, ExecSpec, http};
use crate::{image, net, pipe};
use berth::{Berth, CreateSpec};
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
}

pub fn main() -> anyhow::Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("berthd runs as root");
    }
    // The images and layers are shared by every berth: readable to their
    // namespaces through idmapped mounts, so the directories above them need x.
    for (d, mode) in [
        (PathBuf::from(paths::STATE), 0o711),
        (paths::state("berths"), 0o711),
        (paths::state("images"), 0o711),
        (paths::state("layers"), 0o711),
        (paths::state("repos"), 0o711),
        (PathBuf::from(paths::RUN), 0o755),
        (paths::run("berths"), 0o700),
    ] {
        fs::create_dir_all(&d)?;
        fs::set_permissions(&d, fs::Permissions::from_mode(mode))?;
    }

    fs::create_dir_all(paths::CGROUP)?;
    let _ = fs::write("/sys/fs/cgroup/cgroup.subtree_control", "+cpu +memory +pids");
    fs::write(PathBuf::from(paths::CGROUP).join("cgroup.subtree_control"), "+cpu +memory +pids")
        .context("enabling cgroup controllers for berths")?;
    net::setup_host().context("setting up the berths' network")?;

    let d = Arc::new(Daemon { berths: Mutex::new(load_berths()) });
    {
        let berths = d.berths.lock().unwrap();
        let alive = berths.values().filter(|b| b.alive()).count();
        eprintln!("berthd: {} berths, {alive} running", berths.len());
    }

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
    fn serve(&self, mut conn: UnixStream) {
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
    fn handle(&self, route: Route, body: Value, conn: &mut UnixStream) -> anyhow::Result<Option<Value>> {
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
                let b = self.running(id)?;
                json!({ "ports": net::listening_ports(b.pid) })
            }
            Route::Exec(id) => {
                let spec: ExecSpec = serde_json::from_value(body)?;
                let b = self.running(id)?;
                http::respond_upgrade(conn)?;
                spawn::hand_to_init(&b, &spec, conn.as_raw_fd())?;
                return Ok(None);
            }
            Route::Proxy(id) => {
                let port = body["port"].as_u64().and_then(|p| u16::try_from(p).ok()).context("port missing")?;
                let b = self.running(id)?;
                let tcp = proxy::dial(b.pid, port)?;
                http::respond_upgrade(conn)?;
                pipe::splice(conn.try_clone()?, tcp)?;
                return Ok(None);
            }
            Route::PullImage => {
                let reference = body["reference"].as_str().context("reference missing")?;
                let root = image::pull(&paths::state("images"), reference)?;
                json!({ "reference": reference, "rootfs": root })
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
