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
mod hardening;
mod lifecycle;

use std::io::Write;
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// The layer most tests build on: git, and the agent user.
pub const LAYER: &str = "e2e";

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

/// Builds the shared test layer once per run.
pub fn layer() -> &'static str {
    static BUILT: OnceLock<()> = OnceLock::new();
    BUILT.get_or_init(|| {
        let script = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            script.path(),
            "set -e
            apt-get update -qq
            apt-get install -y -qq --no-install-recommends ca-certificates git sudo >/dev/null
            userdel -r ubuntu 2>/dev/null || true
            useradd -m -u 1000 -s /bin/bash agent
            echo 'agent ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/agent
            rm -rf /var/lib/apt/lists/*",
        )
        .unwrap();
        ok(&["layer", "build", LAYER, &script.path().display().to_string()]);
    });
    LAYER
}

/// A Docker layer on top of the test layer, built once.
pub fn docker_layer() -> &'static str {
    static BUILT: OnceLock<()> = OnceLock::new();
    BUILT.get_or_init(|| {
        layer();
        let script = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            script.path(),
            "set -e
            apt-get update -qq
            apt-get install -y -qq --no-install-recommends docker.io docker-compose-v2 iptables >/dev/null
            usermod -aG docker agent
            rm -rf /var/lib/apt/lists/*",
        )
        .unwrap();
        ok(&["layer", "build", "e2e-docker", "--from", LAYER, &script.path().display().to_string()]);
    });
    "e2e-docker"
}

/// A berth that's destroyed when the test ends, pass or fail.
pub struct Berth(pub String);

impl Berth {
    pub fn new(args: &[&str]) -> Berth {
        layer();
        Berth::new_with_layers(&[LAYER], args)
    }

    pub fn new_with_layers(layers: &[&str], args: &[&str]) -> Berth {
        docker_layer();
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
        wait_for(|| self.sh("cat /proc/net/tcp | grep -q ' 0A '").status.success());
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

/// Fetches / from port through `berth proxy`.
pub fn get(id: &str, port: u16) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_berth"))
        .args(["proxy", id, &port.to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"GET / HTTP/1.0\r\n\r\n").unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).lines().last().unwrap_or("").to_string()
}
