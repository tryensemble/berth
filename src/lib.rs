//! berth: light, isolated boxes for agent threads on one Linux machine.
//!
//! One binary plays three roles: `berth`, the command line; `berthd`, the
//! daemon that makes and runs berths; and `berth __init`, the first process
//! inside each berth. See `docs/ARCHITECTURE.md` for how they fit together.

pub mod api;
pub mod cli;
pub mod daemon;
pub mod image;
pub mod init;
pub mod net;
pub mod pipe;
pub mod sys;
