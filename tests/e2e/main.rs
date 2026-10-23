//! End to end: the real `berth` against a running `berthd`.
//!
//! These need Linux, a daemon (`sudo berthd`), and access to its socket (be in
//! the `berth` group, or set `BERTH_SOCKET`). They're ignored by default:
//!
//! ```sh
//! cargo test --test e2e -- --ignored
//! ```
//!
//! Each test makes its own berths and removes them when it ends, so they run
//! in parallel, as berths on a busy machine would.

mod basics;
mod docker;
mod ensemble;
mod hardening;
mod lifecycle;
mod ports;
mod snapshots;
mod strict;

use std::io::Write;
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub fn berth(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_berth")).args(args).output().expect("running berth")
}

/// Runs berth and returns its stdout, failing the test if it fails.
pub fn ok(args: &[&str]) -> String {
    let out = berth(args);
    assert!(
        out.status.success(),
        "berth {} failed ({}):\n{}{}",
        args.join(" "),
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Builds a layer from script on top of from, named after both so it's
/// rebuilt only when they change, and only once per run. A failed build is
/// remembered: every test that needs it fails with the same message rather
/// than building it again.
pub(crate) fn built(cell: &'static OnceLock<Result<String, String>>, script: &str, from: &[&str]) -> &'static str {
    let result = cell.get_or_init(|| {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (script, from).hash(&mut h);
        let name = format!("e2e-{:08x}", h.finish() as u32);
        if ok(&["layer", "ls"]).lines().any(|l| l == name) {
            return Ok(name);
        }
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), script).unwrap();
        let mut args = vec!["layer", "build", &name];
        for f in from {
            args.extend(["--from", f]);
        }
        let path = file.path().display().to_string();
        args.push(&path);
        let out = berth(&args);
        if out.status.success() {
            Ok(name)
        } else {
            Err(format!("building {name}: {}", String::from_utf8_lossy(&out.stderr)))
        }
    });
    match result {
        Ok(name) => name,
        Err(e) => panic!("{e}"),
    }
}

/// The layer most tests build on: git, sudo, and the agent user.
pub fn layer() -> &'static str {
    static CELL: OnceLock<Result<String, String>> = OnceLock::new();
    built(
        &CELL,
        "set -e
        apt-get update -qq
        apt-get install -y -qq --no-install-recommends ca-certificates git sudo iproute2 >/dev/null
        userdel -r ubuntu 2>/dev/null || true
        useradd -m -u 1000 -s /bin/bash agent
        echo 'agent ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/agent
        rm -rf /var/lib/apt/lists/*",
        &[],
    )
}

/// Docker, on top of the test layer.
pub fn docker_layer() -> &'static str {
    static CELL: OnceLock<Result<String, String>> = OnceLock::new();
    let base = layer();
    built(
        &CELL,
        "set -e
        apt-get update -qq
        apt-get install -y -qq --no-install-recommends docker.io docker-compose-v2 iptables >/dev/null
        usermod -aG docker agent
        rm -rf /var/lib/apt/lists/*",
        &[base],
    )
}

/// A berth that's destroyed when the test ends, pass or fail.
pub struct Berth(pub String);

impl Berth {
    pub fn new(args: &[&str]) -> Berth {
        Berth::new_with_layers(&[layer()], args)
    }

    pub fn new_with_layers(layers: &[&str], args: &[&str]) -> Berth {
        let mut all = vec!["create".to_string()];
        for l in layers {
            all.push("--layer".into());
            all.push((*l).into());
        }
        all.extend(args.iter().map(|a| a.to_string()));
        let argv: Vec<&str> = all.iter().map(String::as_str).collect();
        Berth(ok(&argv))
    }

    /// Runs a shell script in the berth as the agent.
    pub fn sh(&self, script: &str) -> Output {
        berth(&["exec", &self.0, "--", "sh", "-c", script])
    }

    pub fn sh_ok(&self, script: &str) -> String {
        ok(&["exec", &self.0, "--", "sh", "-c", script])
    }

    pub fn root(&self, script: &str) -> String {
        ok(&["exec", "-u", "root", &self.0, "--", "sh", "-c", script])
    }

    pub fn info(&self) -> serde_json::Value {
        let all: serde_json::Value = serde_json::from_str(&ok(&["ls", "--json"])).unwrap();
        all.as_array().unwrap().iter().find(|b| b["id"] == self.0.as_str()).cloned().expect("listed")
    }

    pub fn status(&self) -> String {
        self.info()["status"].as_str().unwrap_or("").to_string()
    }

    /// Serves `text` over HTTP on port, bound to addr, in the background.
    pub fn serve(&self, addr: &str, port: u16, text: &str) {
        // perl is in every Ubuntu image; nothing to install.
        let server = format!(
            r#"use IO::Socket::INET;
            my $s = IO::Socket::INET->new(LocalAddr => "{addr}", LocalPort => {port}, Listen => 5, ReuseAddr => 1) or die $!;
            while (my $c = $s->accept) {{ <$c>; print $c "HTTP/1.0 200 OK\r\n\r\n{text}\n"; close $c }}"#
        );
        ok(&["exec", "-d", "--name", "web", &self.0, "--", "perl", "-e", &server]);
        // Read readiness from the berth's own netns (berth ports), which works
        // whether or not the command runs under gVisor.
        wait_for(|| ok(&["ports", &self.0]).split_whitespace().any(|p| p == port.to_string()));
    }
}

impl Drop for Berth {
    fn drop(&mut self) {
        let _ = berth(&["destroy", &self.0]);
    }
}

pub fn wait_for(f: impl FnMut() -> bool) {
    wait_up_to(Duration::from_secs(10), f)
}

pub fn wait_up_to(limit: Duration, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(start.elapsed() < limit, "timed out");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Fetches / from port through `berth proxy`; empty if nothing answers yet.
pub fn get(id: &str, port: u16) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_berth"))
        .args(["proxy", id, &port.to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // The proxy exits at once when nothing listens; then this write fails.
    let _ = child.stdin.take().unwrap().write_all(b"GET / HTTP/1.0\r\n\r\n");
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).lines().last().unwrap_or("").to_string()
}
