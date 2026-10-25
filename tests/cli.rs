//! The command line on its own, without a daemon.

use std::process::{Command, Output};

fn berth(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_berth"))
        .args(args)
        .env("BERTH_SOCKET", "/nonexistent/berth.sock")
        .output()
        .expect("running berth")
}

#[test]
fn help_lists_every_command() {
    let out = berth(&["--help"]);
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    for cmd in [
        "pull",
        "rmi",
        "repo",
        "layer",
        "create",
        "exec",
        "ports",
        "proxy",
        "forward",
        "desktop",
        "screenshot",
        "destroy",
        "ls",
    ] {
        assert!(help.contains(cmd), "--help doesn't mention {cmd}:\n{help}");
    }
    assert!(!help.contains("__init"), "internal commands are hidden");
}

#[test]
fn version_matches_the_crate() {
    let out = berth(&["--version"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), format!("berth {}", env!("CARGO_PKG_VERSION")));
}

#[test]
fn without_a_daemon_commands_fail_and_say_where_they_looked() {
    let out = berth(&["ls"]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("/nonexistent/berth.sock"), "{err}");
}

#[test]
fn exec_needs_a_command() {
    let out = berth(&["exec", "ab12"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn the_daemon_refuses_to_run_unprivileged() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let out = berth(&["daemon"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("runs as root"));
}
