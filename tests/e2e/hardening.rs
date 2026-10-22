//! The walls a berth's own root can't get through.

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use crate::{Berth, berth, ok};

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
#[cfg(target_arch = "x86_64")]
fn dangerous_kernel_calls_are_refused_even_to_root() {
    let b = Berth::new(&[]);
    // x86_64 numbers: bpf, keyctl, io_uring_setup, userfaultfd, perf_event_open, init_module, kexec_load.
    let calls = [321, 250, 425, 323, 298, 175, 246];
    let script = format!(
        r#"use Errno; for my $n ({}) {{ syscall($n, 0, 0, 0, 0, 0); print "$n ", ($!{{EPERM}} || $!{{ENOSYS}} ? "refused" : "allowed: $!"), "\n" }}"#,
        calls.map(|c| c.to_string()).join(",")
    );
    let out = ok(&["exec", "-u", "root", &b.0, "--", "perl", "-e", &script]);
    for line in out.lines() {
        assert!(line.ends_with("refused"), "{line}");
    }
    assert_eq!(out.lines().count(), calls.len());
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn the_hosts_kernel_views_are_hidden() {
    let b = Berth::new(&[]);
    assert_eq!(b.root("wc -c < /proc/kcore"), "0");
    assert_eq!(b.root("ls /sys/firmware | wc -l"), "0");
    let out = berth(&["exec", "-u", "root", &b.0, "--", "sh", "-c", "echo x > /proc/sys/kernel/domainname"]);
    assert!(!out.status.success(), "kernel tunables are read-only");
    // The berth's own network's tunables are its own.
    b.root("echo 1 > /proc/sys/net/ipv4/ip_forward");
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn a_berth_owns_its_cgroup_but_not_its_limits() {
    let b = Berth::new(&["--memory", "256M"]);
    assert_eq!(b.root("cat /sys/fs/cgroup/cgroup.controllers"), "cpu memory pids");
    b.root("mkdir /sys/fs/cgroup/mine && echo 64M > /sys/fs/cgroup/mine/memory.max");
    for file in ["memory.max", "pids.max", "cpu.max"] {
        let out = berth(&["exec", "-u", "root", &b.0, "--", "sh", "-c", &format!("echo max > /sys/fs/cgroup/{file}")]);
        assert!(!out.status.success(), "the berth raised its {file}");
    }
    assert_eq!(b.root("cat /sys/fs/cgroup/memory.max"), (256u64 << 20).to_string());
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn memory_and_process_limits_hold() {
    let b = Berth::new(&["--memory", "64M", "--pids", "32"]);
    let start = std::time::Instant::now();
    let out = berth(&["exec", &b.0, "--", "perl", "-e", "my $x = 'x' x (256 << 20); print length $x"]);
    assert!(!out.status.success(), "256 MB fit in a 64 MB berth");
    assert!(start.elapsed().as_secs() < 10, "going over the memory limit took {:?} to fail", start.elapsed());
    let fork_until_refused =
        "my $n = 0; for (1..100) { my $p = fork; last unless defined $p; if (!$p) { sleep 30; exit } $n++ } print $n";
    let started: usize = ok(&["exec", &b.0, "--", "perl", "-e", fork_until_refused]).parse().unwrap();
    assert!(started <= 32, "{started} processes in a berth limited to 32");
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn commands_inherit_nothing_from_init() {
    let b = Berth::new(&[]);
    // Just stdin, stdout, stderr, and the directory ls itself opens.
    let fds = b.sh_ok("ls /proc/self/fd");
    assert_eq!(fds.split_whitespace().collect::<Vec<_>>(), ["0", "1", "2", "3"], "{fds}");
    let target = b.sh_ok("readlink /proc/self/fd/3 || true");
    assert!(!target.contains("socket"), "fd 3 is {target}");
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn the_agent_reaches_the_berths_root_through_sudo() {
    let b = Berth::new(&[]);
    assert_eq!(b.sh_ok("id -u; sudo id -u"), "1000\n0");
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn layer_builds_cant_reach_the_host_through_links() {
    let victim = tempfile::tempdir().unwrap();
    std::fs::set_permissions(victim.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(victim.path().join("resolv.conf"), "keep me").unwrap();
    let script = tempfile::NamedTempFile::new().unwrap();
    // Replace the berth's /etc with a link to a host directory, then commit.
    std::fs::write(script.path(), format!("rm -rf /etc && ln -s {} /etc", victim.path().display())).unwrap();
    let name = format!("e2e-links-{}", std::process::id());
    let _ = berth(&["layer", "build", &name, &script.path().display().to_string()]);
    let _ = berth(&["layer", "rm", &name]);
    assert_eq!(std::fs::read_to_string(victim.path().join("resolv.conf")).unwrap(), "keep me");
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn host_users_cant_reach_berths_or_their_layers() {
    let b = Berth::new(&[]);
    for dir in
        ["/var/lib/berth/images".to_string(), "/var/lib/berth/layers".into(), format!("/var/lib/berth/berths/{}", b.0)]
    {
        let e = std::fs::read_dir(&dir).expect_err(&dir);
        assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied, "{dir}");
    }
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn host_users_cant_read_project_mirrors() {
    // A project with a secret in it, mirrored.
    let repo = tempfile::tempdir().unwrap();
    std::fs::set_permissions(repo.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let run =
        |args: &[&str]| assert!(Command::new("git").args(args).current_dir(repo.path()).status().unwrap().success());
    run(&["init", "-q", "-b", "main"]);
    std::fs::write(repo.path().join("secret"), "token=hunter2").unwrap();
    run(&["add", "."]);
    run(&["-c", "user.name=a", "-c", "user.email=a@b", "commit", "-qm", "x"]);
    let project = format!("e2e-secret-{}", std::process::id());
    ok(&["repo", "add", &project, &repo.path().display().to_string()]);

    // This test runs as a berth-group member; a plain host user is uid-only.
    // The repos directory and its contents must not be world-reachable.
    let meta = std::fs::metadata("/var/lib/berth/repos").unwrap();
    assert_eq!(meta.permissions().mode() & 0o077, 0, "repos is reachable by other users");
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn mirrors_are_fetched_without_the_daemons_privileges() {
    // A repository its owner alone can read: the fetch can't either.
    let private = tempfile::tempdir().unwrap();
    let st = Command::new("git").args(["init", "-q", "--bare"]).arg(private.path()).status().unwrap();
    assert!(st.success());
    std::fs::set_permissions(private.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let out =
        berth(&["repo", "add", &format!("e2e-private-{}", std::process::id()), &private.path().display().to_string()]);
    assert!(!out.status.success(), "a private repository was mirrored");

    let out = berth(&["repo", "add", "e2e-inject", "--upload-pack=touch /tmp/berth-pwned"]);
    assert!(!out.status.success());
    assert!(!std::path::Path::new("/tmp/berth-pwned").exists());
}
