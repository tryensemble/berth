//! Idle, stopped and back: berths that nobody uses cost nothing, and come
//! back as they were.

use std::process::{Command, Stdio};
use std::time::Duration;

use crate::{Berth, berth, get, ok, wait_for, wait_up_to};

fn frozen(b: &Berth) -> bool {
    std::fs::read_to_string(format!("/sys/fs/cgroup/berth/{}/cgroup.freeze", b.0)).is_ok_and(|v| v.trim() == "1")
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn idle_berths_freeze_and_wake_at_the_next_use() {
    let b = Berth::new(&[]);
    b.serve("127.0.0.1", 3000, "awake");
    ok(&["idle", &b.0]);
    assert_eq!(b.status(), "idle");
    assert!(frozen(&b));
    // A connection wakes it, and what ran carries on.
    assert_eq!(get(&b.0, 3000), "awake");
    assert_eq!(b.status(), "running");
    assert!(!frozen(&b));
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn stopped_berths_come_back_with_their_files_and_services() {
    let b = Berth::new(&[]);
    let server = r#"use IO::Socket::INET; my $s = IO::Socket::INET->new(LocalPort => 3000, Listen => 5, ReuseAddr => 1) or die $!;
        while (my $c = $s->accept) { <$c>; print $c "HTTP/1.0 200 OK\r\n\r\nservice $$\n"; close $c }"#;
    ok(&["exec", "--service", "web", &b.0, "--", "perl", "-e", server]);
    wait_for(|| get(&b.0, 3000).starts_with("service"));
    b.root("echo kept > /etc/marker");
    b.sh_ok("echo work > /workspace/notes");

    ok(&["stop", &b.0]);
    assert_eq!(b.status(), "stopped");
    assert_eq!(ok(&["ports", &b.0]), "");

    assert_eq!(b.sh_ok("cat /etc/marker /workspace/notes"), "kept\nwork");
    assert_eq!(b.status(), "running");
    wait_for(|| get(&b.0, 3000).starts_with("service"));
    assert!(ok(&["service", "ls", &b.0]).contains("web"));
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn unused_berths_go_idle_by_themselves_and_used_ones_dont() {
    let idle = Berth::new(&["--idle-after", "2s"]);
    let busy = Berth::new(&["--idle-after", "2s"]);
    let attached = Berth::new(&["--idle-after", "2s"]);
    ok(&["exec", "-d", &busy.0, "--", "sh", "-c", "while :; do :; done"]);
    let mut session = Command::new(env!("CARGO_BIN_EXE_berth"))
        .args(["exec", &attached.0, "--", "sleep", "8"])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();

    wait_up_to(Duration::from_secs(15), || idle.status() == "idle");
    assert_eq!(busy.status(), "running", "a berth keeping a CPU busy went idle");
    assert_eq!(attached.status(), "running", "a berth with an open session went idle");
    assert!(session.wait().unwrap().success());
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn services_restart_when_they_exit_until_removed() {
    let b = Berth::new(&[]);
    ok(&["exec", "--service", "flaky", &b.0, "--", "sh", "-c", "echo run >> /tmp/runs; exit 1"]);
    wait_for(|| b.sh_ok("wc -l < /tmp/runs").parse::<u32>().unwrap() >= 3);
    ok(&["service", "rm", &b.0, "flaky"]);
    let after: u32 = b.sh_ok("sleep 1; wc -l < /tmp/runs").parse().unwrap();
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(b.sh_ok("wc -l < /tmp/runs").parse::<u32>().unwrap(), after, "a removed service ran again");
    assert_eq!(ok(&["service", "ls", "--json", &b.0]), "[]");
    let out = berth(&["service", "rm", &b.0, "flaky"]);
    assert!(!out.status.success());
}
