//! Where the daemon keeps things.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Images, layers, mirrors and berths' files, unless BERTH_STATE says
/// otherwise: put it on XFS or btrfs and snapshots and forks are free.
pub const DEFAULT_STATE: &str = "/var/lib/berth";
/// Sockets: the API's and each berth init's.
pub const RUN: &str = "/run/berth";
/// The cgroup every berth's own cgroup sits under.
pub const CGROUP: &str = "/sys/fs/cgroup/berth";

pub fn state_root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| std::env::var_os("BERTH_STATE").map(PathBuf::from).unwrap_or_else(|| DEFAULT_STATE.into()))
}

pub fn state(sub: &str) -> PathBuf {
    state_root().join(sub)
}

pub fn run(sub: &str) -> PathBuf {
    Path::new(RUN).join(sub)
}
