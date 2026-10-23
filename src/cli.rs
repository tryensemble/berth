//! berth, the command line: a client of berthd's socket.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use serde_json::json;

use crate::api::{Client, ExecSpec, frames};
use crate::pipe;

#[derive(Parser)]
#[command(name = "berth", version, about = "Light, isolated boxes for agent threads on one Linux machine")]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Run the daemon (as root)
    Daemon,
    /// Pull an OCI image berths can be made from
    Pull { reference: String },
    /// List pulled images
    Images,
    /// Projects' repositories, mirrored for their berths
    #[command(subcommand)]
    Repo(RepoCmd),
    /// Layers: what a script installed, shared read-only by the berths made with it
    #[command(subcommand)]
    Layer(LayerCmd),
    /// Make a berth; prints its id
    Create {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long, default_value = "ubuntu:24.04")]
        image: String,
        /// A layer to include (repeatable)
        #[arg(long = "layer")]
        layers: Vec<String>,
        #[arg(long, default_value_t = 2.0)]
        cpus: f64,
        #[arg(long, default_value = "4G")]
        memory: String,
        #[arg(long, default_value_t = 4096)]
        pids: u32,
        #[arg(long, default_value = "1G")]
        tmp: String,
        /// Freeze it after this long unused, like 5m (0: never)
        #[arg(long)]
        idle_after: Option<String>,
        /// Stop it after this long unused, like 12h (0: never)
        #[arg(long)]
        stop_after: Option<String>,
        /// Whose berth it is: one tenant's berths share caches, never more
        #[arg(long)]
        tenant: Option<String>,
    },
    /// Run a command in a berth, starting the berth if it's idle or stopped
    Exec {
        id: String,
        /// Give it a terminal
        #[arg(short, long)]
        tty: bool,
        /// KEY=VALUE, or KEY to pass this shell's value on (repeatable)
        #[arg(short, long = "env")]
        env: Vec<String>,
        /// Directory to run in (/workspace by default)
        #[arg(short = 'w', long = "dir", visible_alias = "workdir")]
        workdir: Option<String>,
        /// User in the berth (agent by default)
        #[arg(short, long)]
        user: Option<String>,
        /// Leave it running; output goes to /var/log/berth/NAME.log in the berth
        #[arg(short, long)]
        detach: bool,
        #[arg(long)]
        name: Option<String>,
        /// Run it as a service: kept running, and started again with the berth
        #[arg(long, value_name = "NAME", conflicts_with_all = ["tty", "detach", "name"])]
        service: Option<String>,
        #[arg(trailing_var_arg = true, required = true)]
        cmd: Vec<String>,
    },
    /// Show what a detached command or service printed
    Logs {
        id: String,
        name: String,
        /// Keep printing what it prints
        #[arg(short, long)]
        follow: bool,
        #[arg(short = 'n', long, default_value_t = 100)]
        lines: u32,
    },
    /// Start berths that are idle or stopped
    #[command(visible_alias = "wake")]
    Start { ids: Vec<String> },
    /// Freeze berths: no CPU, their memory pushed out, back at the next use
    Idle { ids: Vec<String> },
    /// Stop berths: nothing runs, their files and services stay
    Stop { ids: Vec<String> },
    /// A berth's services
    #[command(subcommand)]
    Service(ServiceCmd),
    /// Snapshots: a berth's files and services at a moment, kept to go back to
    #[command(subcommand)]
    Snapshot(SnapshotCmd),
    /// Put a berth back as it was in one of its snapshots
    Restore { id: String, name: String },
    /// Make a new berth from a berth, or one of its snapshots; prints its id
    Fork {
        id: String,
        /// Fork from this snapshot instead of the berth as it is now
        #[arg(long)]
        snapshot: Option<String>,
        /// Move the fork's workspace to this branch
        #[arg(long)]
        branch: Option<String>,
    },

    /// List the TCP ports something listens on in a berth
    Ports {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Connect stdin and stdout to a port in a berth
    Proxy { id: String, port: u16 },
    /// Serve a berth's port on this machine's 127.0.0.1
    Forward {
        id: String,
        port: u16,
        /// Local port (the same as the berth's by default)
        #[arg(long)]
        local: Option<u16>,
    },
    /// Print the path of a berth's SOCKS5 socket, where localhost is the berth
    Socks { id: String },
    /// Serve a berth's port on a host port, now and after restarts
    Publish {
        id: String,
        port: u16,
        /// Host port (the same as the berth's by default)
        #[arg(long)]
        host_port: Option<u16>,
        /// Host address to listen on; anyone who can reach it reaches the port
        #[arg(long, default_value = "127.0.0.1")]
        address: String,
    },
    /// Stop serving a berth's port on the host
    Unpublish { id: String, port: u16 },
    /// Remove berths and everything in them
    Destroy { ids: Vec<String> },
    /// List berths
    Ls {
        #[arg(long)]
        json: bool,
    },
    #[command(name = "__init", hide = true)]
    Init,
    #[command(name = "__hold", hide = true)]
    Hold,
}

#[derive(Subcommand)]
pub enum RepoCmd {
    /// Mirror a project's repository, or fetch it again
    Add { name: String, url: String },
    /// List mirrored projects
    Ls,
    /// Remove a project's mirror
    Rm { name: String },
}

#[derive(Subcommand)]
pub enum LayerCmd {
    /// Run a script as root in a fresh berth and keep what it installed
    Build {
        name: String,
        /// Shell script to run
        script: PathBuf,
        #[arg(long, default_value = "ubuntu:24.04")]
        image: String,
        /// Layers to build on top of
        #[arg(long = "from")]
        from: Vec<String>,
    },
    /// List layers
    Ls,
    /// Remove a layer no berth uses
    Rm { name: String },
}

#[derive(Subcommand)]
pub enum SnapshotCmd {
    /// Take a snapshot (the berth is frozen for the moment it takes)
    Create { id: String, name: String },
    /// List a berth's snapshots
    Ls {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Remove a snapshot
    Rm { id: String, name: String },
}

#[derive(Subcommand)]
pub enum ServiceCmd {
    /// List a berth's services
    Ls {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Stop a service and don't start it again
    Rm { id: String, name: String },
}

pub fn main() -> anyhow::Result<i32> {
    run(Cli::parse())
}

/// Runs a parsed command line; returns the exit code.
pub fn run(cli: Cli) -> anyhow::Result<i32> {
    let api = Client::default();
    match cli.cmd {
        Cmd::Daemon => crate::daemon::main().map(|_| 0),
        Cmd::Init => crate::init::main().map(|_| 0),
        Cmd::Hold => {
            crate::daemon::hold();
            Ok(0)
        }
        Cmd::Pull { reference } => {
            let v = api.json("POST", "/v1/images", Some(&json!({ "reference": reference })))?;
            println!("{}", v["rootfs"].as_str().unwrap_or(""));
            Ok(0)
        }
        Cmd::Images => print_names(&api.json("GET", "/v1/images", None)?),
        Cmd::Repo(RepoCmd::Add { name, url }) => {
            api.json("POST", "/v1/repos", Some(&json!({ "name": name, "url": url })))?;
            Ok(0)
        }
        Cmd::Repo(RepoCmd::Ls) => print_names(&api.json("GET", "/v1/repos", None)?),
        Cmd::Repo(RepoCmd::Rm { name }) => {
            api.json("DELETE", &format!("/v1/repos/{name}"), None)?;
            Ok(0)
        }
        Cmd::Layer(LayerCmd::Ls) => print_names(&api.json("GET", "/v1/layers", None)?),
        Cmd::Layer(LayerCmd::Rm { name }) => {
            api.json("DELETE", &format!("/v1/layers/{name}"), None)?;
            Ok(0)
        }
        Cmd::Layer(LayerCmd::Build { name, script, image, from }) => {
            let script = std::fs::read_to_string(&script).with_context(|| format!("reading {}", script.display()))?;
            // Fail before building, not after: a layer berths use can't be replaced.
            let berths = api.json("GET", "/v1/berths", None)?;
            let users: Vec<&str> = berths
                .as_array()
                .into_iter()
                .flatten()
                .filter(|b| b["layers"].as_array().is_some_and(|l| l.iter().any(|l| l == name.as_str())))
                .filter_map(|b| b["id"].as_str())
                .collect();
            if !users.is_empty() {
                bail!("layer {name} is used by berths {}; destroy them first", users.join(", "));
            }
            let b =
                api.json("POST", "/v1/berths", Some(&json!({ "image": image, "layers": from, "idle_after": "0" })))?;
            let id = b["id"].as_str().context("no id")?.to_string();
            eprintln!("berth: building layer {name} in berth {id}");
            let spec = ExecSpec {
                argv: vec!["/bin/sh".into(), "-c".into(), script],
                user: Some("root".into()),
                cwd: Some("/".into()),
                env: vec!["DEBIAN_FRONTEND=noninteractive".into()],
                ..Default::default()
            };
            // The builder berth goes whatever happens; on success, as the layer.
            let built = exec(&api, &id, spec).and_then(|code| {
                if code != 0 {
                    bail!("the script failed with exit code {code}");
                }
                api.json("POST", "/v1/layers", Some(&json!({ "berth": id, "name": name })))
            });
            if let Err(e) = built {
                let _ = api.json("DELETE", &format!("/v1/berths/{id}"), None);
                return Err(e);
            }
            eprintln!("berth: layer {name} is ready");
            Ok(0)
        }
        Cmd::Create { project, branch, image, layers, cpus, memory, pids, tmp, idle_after, stop_after, tenant } => {
            let v = api.json(
                "POST",
                "/v1/berths",
                Some(&json!({
                    "project": project, "branch": branch, "image": image, "layers": layers,
                    "cpus": cpus, "memory": memory, "pids": pids, "tmp": tmp,
                    "idle_after": idle_after, "stop_after": stop_after, "tenant": tenant,
                })),
            )?;
            println!("{}", v["id"].as_str().unwrap_or(""));
            Ok(0)
        }
        Cmd::Exec { id, tty, env, workdir, user, detach, name, service, cmd } => {
            let (rows, cols) = if tty { terminal_size() } else { (0, 0) };
            let env = resolve_env(env, |k| std::env::var(k).ok());
            let detach = detach || service.is_some();
            exec(&api, &id, ExecSpec { argv: cmd, env, cwd: workdir, user, tty, rows, cols, detach, name, service })
        }
        Cmd::Logs { id, name, follow, lines } => {
            if !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) || name.starts_with('.') {
                bail!("bad name {name}");
            }
            let mut argv = vec!["tail".to_string(), "-n".into(), lines.to_string()];
            if follow {
                argv.push("-F".into());
            }
            argv.push(format!("/var/log/berth/{name}.log"));
            exec(&api, &id, ExecSpec { argv, user: Some("root".into()), ..Default::default() })
        }
        Cmd::Start { ids } => each(&api, &ids, "start"),
        Cmd::Idle { ids } => each(&api, &ids, "idle"),
        Cmd::Stop { ids } => each(&api, &ids, "stop"),
        Cmd::Service(ServiceCmd::Ls { id, json }) => {
            let v = api.json("GET", &format!("/v1/berths/{id}/services"), None)?;
            if json {
                println!("{v}");
                return Ok(0);
            }
            println!("{:<16} COMMAND", "NAME");
            for s in v.as_array().into_iter().flatten() {
                let argv: Vec<&str> = s["argv"].as_array().into_iter().flatten().filter_map(|a| a.as_str()).collect();
                println!("{:<16} {}", s["service"].as_str().unwrap_or(""), argv.join(" "));
            }
            Ok(0)
        }
        Cmd::Snapshot(SnapshotCmd::Create { id, name }) => {
            api.json("POST", &format!("/v1/berths/{id}/snapshots"), Some(&json!({ "name": name })))?;
            Ok(0)
        }
        Cmd::Snapshot(SnapshotCmd::Ls { id, json }) => {
            let v = api.json("GET", &format!("/v1/berths/{id}/snapshots"), None)?;
            if json {
                println!("{v}");
                return Ok(0);
            }
            println!("{:<20} CREATED", "NAME");
            for s in v.as_array().into_iter().flatten() {
                let ago = now_secs().saturating_sub(s["created"].as_u64().unwrap_or(0));
                println!("{:<20} {} ago", s["name"].as_str().unwrap_or(""), human_duration(ago));
            }
            Ok(0)
        }
        Cmd::Snapshot(SnapshotCmd::Rm { id, name }) => {
            api.json("DELETE", &format!("/v1/berths/{id}/snapshots/{name}"), None)?;
            Ok(0)
        }
        Cmd::Restore { id, name } => {
            api.json("POST", &format!("/v1/berths/{id}/restore"), Some(&json!({ "name": name })))?;
            Ok(0)
        }
        Cmd::Fork { id, snapshot, branch } => {
            let v = api.json(
                "POST",
                &format!("/v1/berths/{id}/fork"),
                Some(&json!({ "snapshot": snapshot, "branch": branch })),
            )?;
            println!("{}", v["id"].as_str().unwrap_or(""));
            Ok(0)
        }
        Cmd::Service(ServiceCmd::Rm { id, name }) => {
            api.json("DELETE", &format!("/v1/berths/{id}/services/{name}"), None)?;
            Ok(0)
        }
        Cmd::Ports { id, json } => {
            let v = api.json("GET", &format!("/v1/berths/{id}/ports"), None)?;
            if json {
                println!("{}", v["ports"]);
            } else {
                for p in v["ports"].as_array().into_iter().flatten() {
                    println!("{p}");
                }
            }
            Ok(0)
        }
        Cmd::Proxy { id, port } => {
            let conn = proxy(&api, &id, port)?;
            let mut up = conn.try_clone()?;
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut std::io::stdin(), &mut up);
                let _ = up.shutdown(Shutdown::Write);
            });
            std::io::copy(&mut &conn, &mut std::io::stdout())?;
            Ok(0)
        }
        Cmd::Forward { id, port, local } => {
            let local = local.unwrap_or(port);
            let listener = TcpListener::bind(("127.0.0.1", local))?;
            eprintln!("berth: http://127.0.0.1:{local} -> berth {id} port {port}");
            for client in listener.incoming() {
                let Ok(client) = client else { continue };
                let id = id.clone();
                // Each connection on its own, so a slow one holds up no other;
                // one that fails is reported and the forward goes on.
                std::thread::spawn(move || match proxy(&Client::default(), &id, port) {
                    Ok(conn) => {
                        let _ = pipe::splice(client, conn);
                    }
                    Err(e) => eprintln!("berth: {e:#}"),
                });
            }
            Ok(0)
        }
        Cmd::Socks { id } => {
            let v = api.json("GET", &format!("/v1/berths/{id}"), None)?;
            println!("{}", v["socks"].as_str().unwrap_or(""));
            Ok(0)
        }
        Cmd::Publish { id, port, host_port, address } => {
            let v = api.json(
                "POST",
                &format!("/v1/berths/{id}/publish"),
                Some(&json!({ "port": port, "host_port": host_port, "address": address })),
            )?;
            println!("{}:{}", v["address"].as_str().unwrap_or(""), v["host_port"]);
            Ok(0)
        }
        Cmd::Unpublish { id, port } => {
            api.json("DELETE", &format!("/v1/berths/{id}/publish/{port}"), None)?;
            Ok(0)
        }
        Cmd::Destroy { ids } => {
            for id in ids {
                api.json("DELETE", &format!("/v1/berths/{id}"), None)?;
            }
            Ok(0)
        }
        Cmd::Ls { json } => {
            let v = api.json("GET", "/v1/berths", None)?;
            if json {
                println!("{v}");
                return Ok(0);
            }
            println!(
                "{:<10} {:<8} {:>8}  {:<14} {:<12} {:<16} LAYERS",
                "ID", "STATUS", "MEMORY", "ADDRESS", "PROJECT", "BRANCH"
            );
            for b in v.as_array().into_iter().flatten() {
                let s = |k: &str| b[k].as_str().unwrap_or("-").to_string();
                let layers: Vec<String> =
                    b["layers"].as_array().into_iter().flatten().filter_map(|l| l.as_str().map(String::from)).collect();
                let memory = b["memory"].as_u64().map_or("-".into(), human_size);
                println!(
                    "{:<10} {:<8} {:>8}  {:<14} {:<12} {:<16} {}",
                    s("id"),
                    s("status"),
                    memory,
                    s("ip"),
                    s("project"),
                    s("branch"),
                    layers.join(",")
                );
            }
            Ok(0)
        }
    }
}

fn print_names(v: &serde_json::Value) -> anyhow::Result<i32> {
    for n in v.as_array().into_iter().flatten() {
        println!("{}", n.as_str().unwrap_or(""));
    }
    Ok(0)
}

/// Posts action for each berth, printing nothing; stops at the first error.
fn each(api: &Client, ids: &[String], action: &str) -> anyhow::Result<i32> {
    for id in ids {
        api.json("POST", &format!("/v1/berths/{id}/{action}"), None)?;
    }
    Ok(0)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Seconds the way people read them: 40s, 5m, 3h, 2d.
fn human_duration(s: u64) -> String {
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

/// Bytes the way people read them: 812K, 41M, 3.2G.
fn human_size(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 || v >= 10.0 { format!("{v:.0}{}", UNITS[u]) } else { format!("{v:.1}{}", UNITS[u]) }
}

fn proxy(api: &Client, id: &str, port: u16) -> anyhow::Result<UnixStream> {
    api.upgrade(&format!("/v1/berths/{id}/proxy"), &json!({ "port": port }))
}

/// Fills in `KEY` entries from lookup, as `docker run -e KEY` does; a KEY
/// lookup doesn't know is left out.
fn resolve_env(env: Vec<String>, lookup: impl Fn(&str) -> Option<String>) -> Vec<String> {
    env.into_iter()
        .filter_map(|kv| if kv.contains('=') { Some(kv) } else { lookup(&kv).map(|v| format!("{kv}={v}")) })
        .collect()
}

fn terminal_size() -> (u16, u16) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) } == 0 { (ws.ws_row, ws.ws_col) } else { (24, 80) }
}

/// Puts the terminal in raw mode while it lives.
struct Raw(Option<libc::termios>);

impl Raw {
    fn enter() -> Raw {
        unsafe {
            if libc::isatty(0) == 0 {
                return Raw(None);
            }
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return Raw(None);
            }
            let saved = t;
            libc::cfmakeraw(&mut t);
            libc::tcsetattr(0, libc::TCSANOW, &t);
            Raw(Some(saved))
        }
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        if let Some(t) = self.0 {
            unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) };
        }
    }
}

/// Runs spec in a berth, wired to this process's stdio. Returns its exit code.
fn exec(api: &Client, id: &str, spec: ExecSpec) -> anyhow::Result<i32> {
    let detach = spec.detach;
    let tty = spec.tty;
    let mut conn = api.upgrade(&format!("/v1/berths/{id}/exec"), &serde_json::to_value(&spec)?)?;
    let raw = if tty { Raw::enter() } else { Raw(None) };
    if !detach {
        let mut up = conn.try_clone()?;
        std::thread::spawn(move || {
            let mut buf = [0u8; 16 << 10];
            let mut stdin = std::io::stdin();
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = frames::write_frame(&mut up, frames::EOF, &[]);
                        break;
                    }
                    Ok(n) => {
                        if frames::write_frame(&mut up, frames::STDIN, &buf[..n]).is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }
    let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
    let code = loop {
        match frames::read_frame(&mut conn)? {
            Some((frames::STDOUT, d)) => {
                out.write_all(&d)?;
                out.flush()?;
            }
            Some((frames::STDERR, d)) => {
                err.write_all(&d)?;
                err.flush()?;
            }
            Some((frames::STARTED, d)) if detach && d.len() == 4 => {
                println!("{}", u32::from_be_bytes(d.try_into().unwrap()));
                break 0;
            }
            Some((frames::ERROR, d)) => {
                drop(raw);
                eprintln!("berth: {}", String::from_utf8_lossy(&d));
                return Ok(127);
            }
            Some((frames::EXIT, d)) if d.len() == 4 => break i32::from_be_bytes(d.try_into().unwrap()),
            Some(_) => {}
            None => break 255,
        }
    };
    drop(raw);
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_line_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn exec_takes_the_rest_as_the_command() {
        let cli = Cli::try_parse_from(["berth", "exec", "-t", "-e", "A=1", "ab12", "--", "ls", "-la"]).unwrap();
        let Cmd::Exec { id, tty, env, cmd, .. } = cli.cmd else { panic!("not exec") };
        assert_eq!((id.as_str(), tty), ("ab12", true));
        assert_eq!(env, ["A=1"]);
        assert_eq!(cmd, ["ls", "-la"]);
    }

    #[test]
    fn bare_env_names_take_the_callers_value() {
        let lookup = |k: &str| (k == "API_KEY").then(|| "secret=1".to_string());
        let env = resolve_env(vec!["A=1".into(), "API_KEY".into(), "UNSET".into(), "B=".into()], lookup);
        assert_eq!(env, ["A=1", "API_KEY=secret=1", "B="]);
    }

    #[test]
    fn durations_read_like_people_write_them() {
        assert_eq!(human_duration(40), "40s");
        assert_eq!(human_duration(300), "5m");
        assert_eq!(human_duration(3 * 3600 + 5), "3h");
        assert_eq!(human_duration(2 * 86400), "2d");
    }

    #[test]
    fn sizes_read_like_people_write_them() {
        assert_eq!(human_size(512), "512B");
        assert_eq!(human_size(1536), "1.5K");
        assert_eq!(human_size(41 << 20), "41M");
        assert_eq!(human_size((3 << 30) + (1 << 29)), "3.5G");
    }

    #[test]
    fn a_service_cant_also_take_a_terminal() {
        assert!(Cli::try_parse_from(["berth", "exec", "--service", "web", "-t", "ab", "--", "x"]).is_err());
        let cli = Cli::try_parse_from(["berth", "exec", "--service", "web", "--dir", "/srv", "ab", "--", "x"]).unwrap();
        let Cmd::Exec { service, workdir, .. } = cli.cmd else { panic!("not exec") };
        assert_eq!((service.as_deref(), workdir.as_deref()), (Some("web"), Some("/srv")));
    }

    #[test]
    fn create_has_light_defaults() {
        let cli = Cli::try_parse_from(["berth", "create", "--layer", "tools"]).unwrap();
        let Cmd::Create { image, layers, cpus, memory, .. } = cli.cmd else { panic!("not create") };
        assert_eq!((image.as_str(), cpus, memory.as_str()), ("ubuntu:24.04", 2.0, "4G"));
        assert_eq!(layers, ["tools"]);
    }
}
