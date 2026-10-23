//! A berth's cgroup: its limits, delegated to the berth so it can make its
//! own (Docker does), frozen when idle, killed when stopped.
//!
//! ```text
//! /sys/fs/cgroup/berth/<id>        limits, owned by root   the berth's cgroup root
//! /sys/fs/cgroup/berth/<id>/init   init and what it runs   owned by the berth
//! /sys/fs/cgroup/berth/<id>/...    what the berth makes    owned by the berth
//! ```
//!
//! The berth owns its cgroup's directory and process lists, not its limit
//! files, so it can arrange processes below its limits but never raise them.

use std::fs;
use std::os::unix::fs::chown;
use std::path::Path;
use std::time::Duration;

use anyhow::Context;

use super::berth::{Berth, parse_size};

const CONTROLLERS: &str = "+cpu +memory +pids";
/// The files of a cgroup a delegatee needs, per the kernel's cgroup v2 docs.
const DELEGATED: &[&str] = &["cgroup.procs", "cgroup.threads", "cgroup.subtree_control"];

/// Makes the berth's cgroup with its limits, and an empty init cgroup in it.
pub fn create(b: &Berth) -> anyhow::Result<()> {
    let cg = b.cgroup();
    remove(&cg);
    fs::create_dir_all(cg.join("init"))?;
    let mem = parse_size(&b.limits.memory)?;
    let w = |file: &str, v: String| fs::write(cg.join(file), v).with_context(|| format!("setting {file}"));
    w("cpu.max", format!("{} 100000", (b.limits.cpus * 100_000.0) as u64))?;
    // A hard limit: a berth over it has its biggest program killed at once,
    // where a soft one would leave it crawling and looking hung.
    w("memory.max", mem.to_string())?;
    set_swap(b, false);
    w("pids.max", b.limits.pids.to_string())?;
    Ok(())
}

/// Lets the berth's memory go to swap, up to its memory limit, or not at all.
/// Running berths get none, so going over the limit fails fast; idle ones get
/// it, which is where their memory goes. Pages already in swap come back as
/// they're touched either way.
fn set_swap(b: &Berth, allowed: bool) {
    let max =
        if allowed { parse_size(&b.limits.memory).map(|m| m.to_string()).unwrap_or("max".into()) } else { "0".into() };
    let _ = fs::write(b.cgroup().join("memory.swap.max"), max);
}

/// Once init runs in the berth's cgroup: moves it into init/, turns the
/// controllers on for the berth's own cgroups, and hands them to the berth.
pub fn settle(b: &Berth, pid: i32) -> anyhow::Result<()> {
    let cg = b.cgroup();
    fs::write(cg.join("init/cgroup.procs"), pid.to_string()).context("moving init into its cgroup")?;
    fs::write(cg.join("cgroup.subtree_control"), CONTROLLERS).context("enabling the berth's controllers")?;
    let owner = Some(b.uid_base);
    for dir in [cg.clone(), cg.join("init")] {
        chown(&dir, owner, owner)?;
        for f in DELEGATED {
            chown(dir.join(f), owner, owner)?;
        }
    }
    Ok(())
}

pub fn freeze(b: &Berth, frozen: bool) -> anyhow::Result<()> {
    let cg = b.cgroup();
    if !frozen {
        set_swap(b, false);
    }
    fs::write(cg.join("cgroup.freeze"), if frozen { "1" } else { "0" })?;
    let want = if frozen { "frozen 1" } else { "frozen 0" };
    for _ in 0..100 {
        if fs::read_to_string(cg.join("cgroup.events")).unwrap_or_default().contains(want) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    anyhow::bail!("berth {} didn't {}", b.id, if frozen { "freeze" } else { "thaw" })
}

pub fn frozen(b: &Berth) -> bool {
    fs::read_to_string(b.cgroup().join("cgroup.freeze")).is_ok_and(|v| v.trim() == "1")
}

/// Pushes the berth's memory out, to swap where there is some. Best effort:
/// the kernel stops at what it can't reclaim.
pub fn reclaim(b: &Berth) {
    let cg = b.cgroup();
    set_swap(b, true);
    if let Some(current) = memory_current(b) {
        let _ = fs::write(cg.join("memory.reclaim"), current.to_string());
    }
}

/// Hands back what the berth's memory holds that can be dropped (file cache,
/// mostly) without freezing it or touching its programs' own memory: running
/// berths have no swap to push that to. Pages still being written need
/// writing first, so it takes a few passes while memory keeps going down.
pub fn trim(b: &Berth) {
    // Dirty pages can't be dropped until they're written, which the kernel
    // otherwise does within 30 s: write them now. syncfs writes the whole
    // filesystem the berth's files are on, which is what an explicit trim
    // can afford.
    if let Ok(dir) = fs::File::open(b.dir()) {
        use std::os::fd::AsRawFd;
        unsafe { libc::syncfs(dir.as_raw_fd()) };
    }
    let mut last = u64::MAX;
    for _ in 0..5 {
        let Some(current) = memory_current(b) else { return };
        if current >= last || current < 1 << 20 {
            return;
        }
        last = current;
        let _ = fs::write(b.cgroup().join("memory.reclaim"), current.to_string());
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn memory_current(b: &Berth) -> Option<u64> {
    fs::read_to_string(b.cgroup().join("memory.current")).ok()?.trim().parse().ok()
}

/// CPU time the berth has used, in microseconds.
pub fn cpu_usage(b: &Berth) -> Option<u64> {
    let stat = fs::read_to_string(b.cgroup().join("cpu.stat")).ok()?;
    stat.lines().find_map(|l| l.strip_prefix("usage_usec ")).and_then(|v| v.trim().parse().ok())
}

/// Ends every process in the berth, Docker's included. Returns whether the
/// cgroup really emptied: a caller must not touch the berth's files until it
/// has, since a surviving process could redirect the daemon's writes.
#[must_use]
pub fn kill(b: &Berth) -> bool {
    let cg = b.cgroup();
    if !cg.exists() {
        return true;
    }
    // A frozen cgroup still dies, but thawing first lets it go at once.
    let _ = fs::write(cg.join("cgroup.freeze"), "0");
    // Up to 5 s: a container with a long SIGKILL path (fuse, say) can be slow.
    for _ in 0..250 {
        let _ = fs::write(cg.join("cgroup.kill"), "1");
        if fs::read_to_string(cg.join("cgroup.events")).unwrap_or_default().contains("populated 0") {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Removes an empty cgroup tree, deepest first.
pub fn remove(cg: &Path) {
    if let Ok(rd) = fs::read_dir(cg) {
        for e in rd.flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                remove(&e.path());
            }
        }
    }
    let _ = fs::remove_dir(cg);
}
