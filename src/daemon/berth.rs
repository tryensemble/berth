//! A berth's record: what it was made from, its limits, where it lives.

use std::fs::{self, File};
use std::io::Read;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::paths;
use crate::api::ExecSpec;
use crate::sys;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Limits {
    pub cpus: f64,
    pub memory: String,
    pub pids: u32,
    pub tmp: String,
}

/// What a caller asks for when making a berth.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
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
    /// Freeze the berth after this long unused, like `5m`; `0` never does.
    pub idle_after: Option<String>,
    /// Stop it after this long unused, like `12h`; `0` never does.
    pub stop_after: Option<String>,
    /// Whose berth it is: berths of one tenant share caches and a registry
    /// mirror, never anything else.
    pub tenant: Option<String>,
}

/// How long a berth may go unused before it's frozen, and before it's stopped.
pub const DEFAULT_IDLE_AFTER: u64 = 5 * 60;
pub const DEFAULT_STOP_AFTER: u64 = 12 * 60 * 60;

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

/// Where a berth is in its life.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Its programs run.
    #[default]
    Running,
    /// Frozen, its memory pushed out: no CPU, a few MB, back instantly.
    Idle,
    /// Nothing runs; only its files are kept. Back in a second, with its
    /// services.
    Stopped,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Running => "running",
            State::Idle => "idle",
            State::Stopped => "stopped",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Berth {
    pub id: String,
    /// Its number on this machine, which sets its address and id range.
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
    #[serde(default)]
    pub state: State,
    /// Seconds unused before it's frozen and before it's stopped; 0 is never.
    #[serde(default = "default_idle_after")]
    pub idle_after: u64,
    #[serde(default = "default_stop_after")]
    pub stop_after: u64,
    /// What runs again whenever the berth starts.
    #[serde(default)]
    pub services: Vec<ExecSpec>,
    #[serde(default)]
    pub tenant: Option<String>,
    /// Its ports served on the host.
    #[serde(default)]
    pub published: Vec<super::proxy::Published>,
}

fn default_idle_after() -> u64 {
    DEFAULT_IDLE_AFTER
}
fn default_stop_after() -> u64 {
    DEFAULT_STOP_AFTER
}

impl Berth {
    pub fn dir(&self) -> PathBuf {
        paths::state("berths").join(&self.id)
    }

    pub fn run_dir(&self) -> PathBuf {
        paths::run("berths").join(&self.id)
    }

    pub fn cgroup(&self) -> PathBuf {
        PathBuf::from(paths::CGROUP).join(&self.id)
    }

    /// Whether its PID 1 still runs, and is still ours (pids get reused).
    pub fn alive(&self) -> bool {
        sys::pid_alive(self.pid)
            && fs::read_to_string(format!("/proc/{}/cgroup", self.pid))
                .map(|c| c.contains(&format!("/berth/{}", self.id)))
                .unwrap_or(false)
    }

    pub fn save(&self) -> anyhow::Result<()> {
        fs::write(self.dir().join("state.json"), serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    /// The berth as the API shows it, with what it uses now.
    pub fn view(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap();
        let state = match self.state {
            State::Running | State::Idle if !self.alive() => State::Stopped,
            s => s,
        };
        v["status"] = json!(state.as_str());
        v["memory"] = json!(super::cgroup::memory_current(self));
        v["socks"] = json!(super::proxy::socks_path(&self.id));
        v
    }
}

/// A size like `512M` or `4G`, in bytes.
pub fn parse_size(s: &str) -> anyhow::Result<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('K' | 'k') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M' | 'm') => (&s[..s.len() - 1], 1 << 20),
        Some('G' | 'g') => (&s[..s.len() - 1], 1 << 30),
        _ => (s, 1),
    };
    let n: f64 = num.parse().with_context(|| format!("bad size {s}"))?;
    anyhow::ensure!(n.is_finite() && n >= 0.0, "bad size {s}");
    Ok((n * mult as f64) as u64)
}

/// A duration like `90`, `30s`, `5m`, `12h` or `2d`, in seconds.
pub fn parse_duration(s: &str) -> anyhow::Result<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('h') => (&s[..s.len() - 1], 3600),
        Some('d') => (&s[..s.len() - 1], 86400),
        _ => (s, 1),
    };
    let n: u64 = num.parse().with_context(|| format!("bad duration {s}"))?;
    n.checked_mul(mult).with_context(|| format!("bad duration {s}"))
}

/// A short random id, like `6ae0756e`.
pub fn random_id() -> String {
    let mut b = [0u8; 4];
    File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).expect("urandom");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse() {
        assert_eq!(parse_size("4G").unwrap(), 4 << 30);
        assert_eq!(parse_size("512m").unwrap(), 512 << 20);
        assert_eq!(parse_size("1.5K").unwrap(), 1536);
        assert_eq!(parse_size(" 100 ").unwrap(), 100);
        for bad in ["", "G", "lots", "-1G", "NaNG"] {
            assert!(parse_size(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn durations_parse() {
        assert_eq!(parse_duration("90").unwrap(), 90);
        assert_eq!(parse_duration("30s").unwrap(), 30);
        assert_eq!(parse_duration("5m").unwrap(), 300);
        assert_eq!(parse_duration("12h").unwrap(), 43200);
        assert_eq!(parse_duration("2d").unwrap(), 172800);
        assert_eq!(parse_duration("0").unwrap(), 0);
        for bad in ["", "m", "-5m", "1.5h", "5w", "99999999999999999999d"] {
            assert!(parse_duration(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn records_from_before_states_load_as_running() {
        let old = r#"{"id":"ab","n":2,"uid_base":1131072,"ip":"10.200.0.2","pid":0,"project":null,"branch":null,
            "image":"ubuntu:24.04","layers":[],"limits":{"cpus":2.0,"memory":"4G","pids":4096,"tmp":"1G"},"created":0}"#;
        let b: Berth = serde_json::from_str(old).unwrap();
        assert_eq!(b.state, State::Running);
        assert_eq!((b.idle_after, b.stop_after), (DEFAULT_IDLE_AFTER, DEFAULT_STOP_AFTER));
        assert!(b.services.is_empty());
    }

    #[test]
    fn create_spec_defaults() {
        let spec: CreateSpec = serde_json::from_str("{}").unwrap();
        assert_eq!(spec.image, "ubuntu:24.04");
        assert_eq!((spec.cpus, spec.memory.as_str(), spec.pids, spec.tmp.as_str()), (2.0, "4G", 4096, "1G"));
        assert!(spec.layers.is_empty() && spec.project.is_none());
    }

    #[test]
    fn ids_are_eight_hex_digits() {
        let (a, b) = (random_id(), random_id());
        assert_eq!(a.len(), 8);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
