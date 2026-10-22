//! The prototype's promises: a berth is a machine of its own, and berths
//! don't meet.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::{Berth, berth, get, ok, wait_for};

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn a_berth_is_its_own_machine() {
    let b = Berth::new(&[]);
    assert_eq!(b.sh_ok("hostname"), format!("berth-{}", b.0));
    assert_eq!(b.sh_ok("id -un; pwd"), "agent\n/workspace");
    assert_eq!(b.root("id -u"), "0");
    // Its init is PID 1, and nothing from the host is in sight.
    assert_eq!(b.sh_ok("cat /proc/1/cmdline | tr '\\0' ' '"), "/usr/local/bin/berth __init");
    let procs: usize = b.sh_ok("ls /proc | grep -c '^[0-9]'").parse().unwrap();
    assert!(procs < 10, "{procs} processes visible");
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn root_in_a_berth_is_nobody_on_the_host() {
    let b = Berth::new(&[]);
    let pid = b.info()["pid"].as_i64().unwrap();
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let uid: u32 = status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!(uid >= 1_000_000, "the berth's root is host uid {uid}");
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn exec_carries_exit_codes_input_env_and_dir() {
    let b = Berth::new(&[]);
    assert_eq!(b.sh("exit 42").status.code(), Some(42));

    let out = berth(&[
        "exec",
        "-e",
        "GREETING=hi",
        "-w",
        "/tmp",
        &b.0,
        "--",
        "sh",
        "-c",
        "echo $GREETING; pwd; echo oops >&2",
    ]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi\n/tmp\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "oops\n");

    let mut child = Command::new(env!("CARGO_BIN_EXE_berth"))
        .args(["exec", &b.0, "--", "tr", "a-z", "A-Z"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"through stdin").unwrap();
    assert_eq!(child.wait_with_output().unwrap().stdout, b"THROUGH STDIN");

    let out = Command::new(env!("CARGO_BIN_EXE_berth"))
        .args(["exec", "-e", "FROM_CALLER", &b.0, "--", "sh", "-c", "echo $FROM_CALLER"])
        .env("FROM_CALLER", "passed on")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "passed on\n");

    let out = berth(&["exec", &b.0, "--", "/no/such/command"]);
    assert_eq!(out.status.code(), Some(127));
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn two_berths_serve_the_same_port_without_colliding() {
    let (a, b) = (Berth::new(&[]), Berth::new(&[]));
    a.serve("127.0.0.1", 3000, "from a");
    b.serve("127.0.0.1", 3000, "from b");
    assert_eq!(ok(&["ports", &a.0]), "3000");
    assert_eq!(ok(&["ports", &b.0]), "3000");
    assert_eq!(get(&a.0, 3000), "from a");
    assert_eq!(get(&b.0, 3000), "from b");
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn berths_cant_reach_each_other_the_host_or_metadata() {
    let (a, b) = (Berth::new(&[]), Berth::new(&[]));
    b.serve("0.0.0.0", 8080, "open to all");
    let b_ip = b.info()["ip"].as_str().unwrap().to_string();
    assert_eq!(b.sh_ok(&format!("timeout 3 bash -c 'echo > /dev/tcp/{b_ip}/8080' && echo reached")), "reached");

    for (what, target) in [
        ("berth b", format!("{b_ip}/8080")),
        ("the host", "10.200.0.1/22".into()),
        ("metadata", "169.254.169.254/80".into()),
    ] {
        let out = a.sh(&format!("timeout 3 bash -c 'echo > /dev/tcp/{target}'"));
        assert!(!out.status.success(), "berth a reached {what}");
    }
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn writes_stay_in_their_berth_and_layers_stay_read_only() {
    let (a, b) = (Berth::new(&[]), Berth::new(&[]));
    a.root("echo a > /etc/only-in-a; apt-get --version >/dev/null && rm /usr/bin/apt-get");
    assert_eq!(a.sh("test -e /usr/bin/apt-get").status.code(), Some(1));
    assert_eq!(b.sh("test -e /etc/only-in-a").status.code(), Some(1));
    assert!(b.sh("test -x /usr/bin/apt-get").status.success());
    // The agent can't change the system, only its workspace and /tmp.
    assert!(!a.sh("touch /etc/agent-was-here").status.success());
    a.sh_ok("touch /workspace/mine /tmp/mine");
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn detached_commands_outlive_their_exec_and_log() {
    let b = Berth::new(&[]);
    let pid = ok(&["exec", "-d", "--name", "ticker", &b.0, "--", "sh", "-c", "echo started; sleep 300"]);
    assert!(pid.parse::<u32>().is_ok(), "{pid}");
    wait_for(|| b.sh("grep -q started /var/log/berth/ticker.log").status.success());
    assert!(b.sh(&format!("kill -0 {pid}")).status.success());
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn projects_are_checked_out_on_their_branch() {
    let repo = tempfile::tempdir().unwrap();
    // Mirrors are fetched as an unprivileged id: the repository must be readable.
    std::fs::set_permissions(repo.path(), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let git = |args: &[&str]| {
        let st = Command::new("git").args(args).current_dir(repo.path()).status().unwrap();
        assert!(st.success(), "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(repo.path().join("README"), "e2e\n").unwrap();
    git(&["add", "."]);
    git(&["-c", "user.name=e2e", "-c", "user.email=e2e@example.com", "commit", "-qm", "init"]);

    let project = format!("e2e-{}", std::process::id());
    ok(&["repo", "add", &project, &repo.path().display().to_string()]);
    let b = Berth::new(&["--project", &project, "--branch", "thread-1"]);
    assert_eq!(b.sh_ok("git branch --show-current; cat README"), "thread-1\ne2e");
    // The shared mirror can't be written from the berth.
    assert!(!b.sh("touch /berth/repo.git/HEAD").status.success());
    assert_eq!(b.sh_ok("git remote get-url origin"), repo.path().display().to_string());
    b.sh_ok("git -c user.name=a -c user.email=a@b commit -q --allow-empty -m mine");
    // The mirror can't go while a berth uses it.
    assert!(!berth(&["repo", "rm", &project]).status.success());
    drop(b);
    ok(&["repo", "rm", &project]);
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn destroyed_berths_are_gone() {
    let b = Berth::new(&[]);
    let id = b.0.clone();
    ok(&["destroy", &id]);
    assert!(!ok(&["ls"]).contains(&id));
    let out = berth(&["exec", &id, "--", "true"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no berth"));
    assert!(!Path::new(&format!("/run/berth/berths/{id}")).exists());
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn bad_requests_are_refused() {
    let out = berth(&["create", "--layer", "no-such-layer"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("no layer no-such-layer"));
    let out = berth(&["repo", "add", "../escape", "/tmp"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("bad project name"));
}
