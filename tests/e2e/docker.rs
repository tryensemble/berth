//! Docker inside a berth: its own, started on first use, and no other berth's.

use crate::{Berth, get, ok, wait_up_to};
use std::time::Duration;

/// Docker pulls images over the network under parallel load; give it room.
const SLOW: Duration = Duration::from_secs(60);

/// A berth with git, sudo and Docker.
fn docker_berth() -> Berth {
    Berth::new_with_layers(&[crate::layer(), crate::docker_layer()], &["--memory", "3G"])
}

#[test]
#[ignore = "needs a running berthd with the e2e-docker layer; run with --ignored"]
fn a_berth_runs_its_own_containers() {
    let b = docker_berth();
    // Docker starts on first use, not before.
    assert!(!b.sh("pgrep -x dockerd").status.success(), "dockerd runs before Docker is used");
    assert_eq!(b.sh_ok("docker run --rm hello-world 2>/dev/null | grep -c Hello"), "1");
    assert!(b.sh("pgrep -x dockerd").status.success(), "dockerd didn't start on use");
}

#[test]
#[ignore = "needs a running berthd with the e2e-docker layer; run with --ignored"]
fn two_berths_dockers_dont_meet() {
    let (a, b) = (docker_berth(), docker_berth());
    for x in [&a, &b] {
        x.sh_ok("docker run -d --name srv -p 5000:80 busybox sh -c 'while true; do printf \"HTTP/1.0 200 OK\\r\\n\\r\\n$(hostname)\\n\" | nc -l -p 80; done' >/dev/null");
    }
    wait_up_to(SLOW, || !get(&a.0, 5000).is_empty() && !get(&b.0, 5000).is_empty());
    // Each berth sees only its own container, on its own :5000.
    assert_eq!(a.sh_ok("docker ps --format '{{.Names}}'"), "srv");
    assert_ne!(get(&a.0, 5000), get(&b.0, 5000));
    assert_eq!(ok(&["ports", &a.0]), "5000");
}

#[test]
#[ignore = "needs a running berthd with the e2e-docker layer; run with --ignored"]
fn containers_count_against_the_berths_memory() {
    let b = docker_berth();
    // The berth's cgroup sees the container's processes.
    b.sh_ok("docker run -d --name sleeper busybox sleep 300 >/dev/null");
    wait_up_to(SLOW, || {
        b.sh("docker inspect -f '{{.State.Running}}' sleeper 2>/dev/null | grep -q true").status.success()
    });
    let pids: u32 = b.root("cat /sys/fs/cgroup/pids.current").parse().unwrap();
    assert!(pids > 1, "the container's processes aren't in the berth's cgroup");
}

#[test]
#[ignore = "needs a running berthd with the e2e-docker layer; run with --ignored"]
fn docker_containers_come_back_after_a_stop() {
    let b = docker_berth();
    b.sh_ok("docker run -d --name keeper --restart always -p 5000:80 busybox sh -c 'while true; do printf \"HTTP/1.0 200 OK\\r\\n\\r\\nalive\\n\" | nc -l -p 80; done' >/dev/null");
    wait_up_to(SLOW, || get(&b.0, 5000) == "alive");
    ok(&["stop", &b.0]);
    assert_eq!(b.status(), "stopped");
    // Waking the berth brings dockerd and the restart-policy container back.
    ok(&["start", &b.0]);
    wait_up_to(SLOW, || get(&b.0, 5000) == "alive");
}

#[test]
#[ignore = "needs a running berthd with the e2e-docker layer; run with --ignored"]
fn docker_pulls_go_through_the_tenants_own_mirror() {
    let tenant = format!("e2e-t{}", std::process::id());
    let ours =
        Berth::new_with_layers(&[crate::layer(), crate::docker_layer()], &["--memory", "3G", "--tenant", &tenant]);
    let theirs = docker_berth();
    let mirror_of = |b: &Berth| b.info()["registry"].as_u64().expect("a Docker berth has a registry mirror");
    let (our_port, their_port) = (mirror_of(&ours), mirror_of(&theirs));
    assert_ne!(our_port, their_port, "two tenants share a mirror");
    // A caller can't take a registry mirror's name to stand in for one.
    assert!(!crate::berth(&["create", "--layer", crate::layer(), "--name", &format!("berth-registry-{tenant}")]).status.success());
    assert!(ours.sh_ok("docker info 2>/dev/null").contains(&format!("http://10.200.0.1:{our_port}/")));
    // A berth reaches its own tenant's mirror and not another's.
    let reach =
        |b: &Berth, port: u64| b.sh(&format!("timeout 3 bash -c 'echo > /dev/tcp/10.200.0.1/{port}'")).status.success();
    assert!(reach(&ours, our_port));
    assert!(!reach(&ours, their_port), "a berth reached another tenant's mirror");
    // And its pulls go through it: the mirror berth exists for this tenant,
    // and the pull reaches it (whether the pull itself succeeds depends on
    // the upstream, which may rate-limit a shared host).
    assert!(ok(&["ls", "--json"]).contains(&format!("berth-registry-{tenant}")));
    let pull = ours.sh("docker pull -q busybox 2>&1");
    let out = String::from_utf8_lossy(&pull.stdout);
    assert!(pull.status.success() || out.contains("Too Many Requests"), "pull didn't reach the mirror: {out}");
    // The tenant's mirror goes when we're done; it's made again when needed.
    drop(ours);
    let _ = crate::berth(&["destroy", &format!("berth-registry-{tenant}")]);
}
