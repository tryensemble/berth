//! Where the daemon keeps things.

use std::path::{Path, PathBuf};

/// Images, layers, mirrors and berths' files.
pub const STATE: &str = "/var/lib/berth";
/// Sockets: the API's and each berth init's.
pub const RUN: &str = "/run/berth";
/// The cgroup every berth's own cgroup sits under.
pub const CGROUP: &str = "/sys/fs/cgroup/berth";

pub fn state(sub: &str) -> PathBuf {
    Path::new(STATE).join(sub)
}

pub fn run(sub: &str) -> PathBuf {
    Path::new(RUN).join(sub)
}
