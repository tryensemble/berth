//! The API between `berth` and `berthd`: a small HTTP/1.1 over a Unix socket,
//! and the frames an exec or proxy stream speaks once the connection is
//! upgraded.

mod client;
pub mod frames;
pub mod http;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub use client::Client;
pub use frames::Frames;

/// Where the daemon listens unless `BERTH_SOCKET` says otherwise.
pub const DEFAULT_SOCKET: &str = "/run/berth/berth.sock";

/// The daemon's socket: `BERTH_SOCKET`, or [`DEFAULT_SOCKET`].
pub fn socket_path() -> PathBuf {
    std::env::var_os("BERTH_SOCKET").map(PathBuf::from).unwrap_or_else(|| DEFAULT_SOCKET.into())
}

/// A command to run in a berth.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ExecSpec {
    pub argv: Vec<String>,
    /// `KEY=VALUE` pairs, on top of the berth's defaults.
    #[serde(default)]
    pub env: Vec<String>,
    pub cwd: Option<String>,
    /// A user name or uid in the berth; `agent` when the berth has one.
    pub user: Option<String>,
    #[serde(default)]
    pub tty: bool,
    #[serde(default)]
    pub rows: u16,
    #[serde(default)]
    pub cols: u16,
    /// Leave it running; its output goes to `/var/log/berth/<name>.log`.
    #[serde(default)]
    pub detach: bool,
    pub name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_spec_fills_in_defaults() {
        let spec: ExecSpec = serde_json::from_str(r#"{"argv":["true"]}"#).unwrap();
        assert_eq!(spec, ExecSpec { argv: vec!["true".into()], ..Default::default() });
    }
}
