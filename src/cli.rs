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
    /// Add a project's repository, or fetch it again
    Repo { name: String, url: String },
    /// Build a layer: run a script as root in a fresh berth and keep what it installed
    Layer {
        name: String,
        /// Shell script to run
        script: PathBuf,
        #[arg(long, default_value = "ubuntu:24.04")]
        image: String,
        /// Layers to build on top of
        #[arg(long = "from")]
        from: Vec<String>,
    },
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
    },
    /// Run a command in a berth
    Exec {
        id: String,
        /// Give it a terminal
        #[arg(short, long)]
        tty: bool,
        /// KEY=VALUE (repeatable)
        #[arg(short, long = "env")]
        env: Vec<String>,
        #[arg(short = 'w', long = "workdir")]
        workdir: Option<String>,
        /// User in the berth (agent by default)
        #[arg(short, long)]
        user: Option<String>,
        /// Leave it running; output goes to /var/log/berth/NAME.log in the berth
        #[arg(short, long)]
        detach: bool,
        #[arg(long)]
        name: Option<String>,
        #[arg(trailing_var_arg = true, required = true)]
        cmd: Vec<String>,
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
        Cmd::Repo { name, url } => {
            api.json("POST", "/v1/repos", Some(&json!({ "name": name, "url": url })))?;
            Ok(0)
        }
        Cmd::Layer { name, script, image, from } => {
            let script = std::fs::read_to_string(&script).with_context(|| format!("reading {}", script.display()))?;
            let b = api.json("POST", "/v1/berths", Some(&json!({ "image": image, "layers": from })))?;
            let id = b["id"].as_str().context("no id")?.to_string();
            eprintln!("berth: building layer {name} in berth {id}");
            let spec = ExecSpec {
                argv: vec!["/bin/sh".into(), "-c".into(), script],
                user: Some("root".into()),
                cwd: Some("/".into()),
                env: vec!["DEBIAN_FRONTEND=noninteractive".into()],
                ..Default::default()
            };
            let code = exec(&api, &id, spec)?;
            if code != 0 {
                let _ = api.json("DELETE", &format!("/v1/berths/{id}"), None);
                bail!("the script failed with exit code {code}");
            }
            api.json("POST", "/v1/layers", Some(&json!({ "berth": id, "name": name })))?;
            eprintln!("berth: layer {name} is ready");
            Ok(0)
        }
        Cmd::Create { project, branch, image, layers, cpus, memory, pids, tmp } => {
            let v = api.json(
                "POST",
                "/v1/berths",
                Some(&json!({
                    "project": project, "branch": branch, "image": image, "layers": layers,
                    "cpus": cpus, "memory": memory, "pids": pids, "tmp": tmp,
                })),
            )?;
            println!("{}", v["id"].as_str().unwrap_or(""));
            Ok(0)
        }
        Cmd::Exec { id, tty, env, workdir, user, detach, name, cmd } => {
            let (rows, cols) = if tty { terminal_size() } else { (0, 0) };
            exec(&api, &id, ExecSpec { argv: cmd, env, cwd: workdir, user, tty, rows, cols, detach, name })
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
            println!("{:<10} {:<8} {:<14} {:<12} {:<16} LAYERS", "ID", "STATUS", "ADDRESS", "PROJECT", "BRANCH");
            for b in v.as_array().into_iter().flatten() {
                let s = |k: &str| b[k].as_str().unwrap_or("-").to_string();
                let layers: Vec<String> =
                    b["layers"].as_array().into_iter().flatten().filter_map(|l| l.as_str().map(String::from)).collect();
                println!(
                    "{:<10} {:<8} {:<14} {:<12} {:<16} {}",
                    s("id"),
                    s("status"),
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

fn proxy(api: &Client, id: &str, port: u16) -> anyhow::Result<UnixStream> {
    api.upgrade(&format!("/v1/berths/{id}/proxy"), &json!({ "port": port }))
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
    fn create_has_light_defaults() {
        let cli = Cli::try_parse_from(["berth", "create", "--layer", "tools"]).unwrap();
        let Cmd::Create { image, layers, cpus, memory, .. } = cli.cmd else { panic!("not create") };
        assert_eq!((image.as_str(), cpus, memory.as_str()), ("ubuntu:24.04", 2.0, "4G"));
        assert_eq!(layers, ["tools"]);
    }
}
