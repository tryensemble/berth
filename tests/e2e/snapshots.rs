//! Snapshots, restore and fork: a berth's files and services at a moment.

use std::process::Command;

use crate::{Berth, berth, get, ok, wait_for};

fn serve_service(b: &Berth, port: u16, text: &str) {
    let server = format!(
        r#"use IO::Socket::INET; my $s = IO::Socket::INET->new(LocalPort => {port}, Listen => 5, ReuseAddr => 1) or die $!;
        while (my $c = $s->accept) {{ <$c>; print $c "HTTP/1.0 200 OK\r\n\r\n{text}\n"; close $c }}"#
    );
    ok(&["exec", "--service", "web", &b.0, "--", "perl", "-e", &server]);
    wait_for(|| get(&b.0, port) == text);
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn a_restored_berth_is_as_it_was() {
    let b = Berth::new(&[]);
    b.sh_ok("echo before > /workspace/notes");
    b.root("echo before > /etc/marker");
    serve_service(&b, 3000, "v1");
    ok(&["snapshot", "create", &b.0, "s1"]);
    assert!(ok(&["snapshot", "ls", &b.0]).contains("s1"));

    b.sh_ok("echo after > /workspace/notes; rm -f /workspace/gone");
    b.root("echo after > /etc/marker; apt-get --version >/dev/null && rm /usr/bin/apt-get");
    ok(&["service", "rm", &b.0, "web"]);

    ok(&["restore", &b.0, "s1"]);
    assert_eq!(b.sh_ok("cat /workspace/notes /etc/marker"), "before\nbefore");
    assert!(b.sh("test -x /usr/bin/apt-get").status.success());
    // The service it had then runs again.
    wait_for(|| get(&b.0, 3000) == "v1");
    // A snapshot can be restored more than once.
    b.sh_ok("echo again > /workspace/notes");
    ok(&["restore", &b.0, "s1"]);
    assert_eq!(b.sh_ok("cat /workspace/notes"), "before");
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn a_fork_is_a_berth_of_its_own() {
    let b = Berth::new(&[]);
    b.sh_ok("echo shared > /workspace/notes");
    b.root("echo shared > /etc/marker");
    serve_service(&b, 3000, "served");
    let fork = Berth(ok(&["fork", &b.0]));
    assert_ne!(fork.0, b.0);

    assert_eq!(fork.sh_ok("cat /workspace/notes /etc/marker; hostname"), format!("shared\nshared\nberth-{}", fork.0));
    // Its files belong to its own ids, not the source's.
    let pid = fork.info()["pid"].as_i64().unwrap();
    let base = fork.info()["uid_base"].as_u64().unwrap();
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    assert!(status.contains(&format!("Uid:\t{base}")), "the fork's init isn't its own root");
    assert_eq!(fork.sh_ok("stat -c %u /workspace/notes"), "1000");
    // Its services came along, on its own port 3000.
    wait_for(|| get(&fork.0, 3000) == "served");
    // And from here on the two are apart.
    fork.sh_ok("echo mine > /workspace/notes");
    assert_eq!(b.sh_ok("cat /workspace/notes"), "shared");
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn a_fork_from_a_snapshot_starts_from_then() {
    let b = Berth::new(&[]);
    b.sh_ok("echo then > /workspace/notes");
    ok(&["snapshot", "create", &b.0, "then"]);
    b.sh_ok("echo now > /workspace/notes");
    let fork = Berth(ok(&["fork", "--snapshot", "then", &b.0]));
    assert_eq!(fork.sh_ok("cat /workspace/notes"), "then");
    assert_eq!(b.sh_ok("cat /workspace/notes"), "now");
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn a_project_fork_moves_to_its_own_branch() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::set_permissions(repo.path(), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let git =
        |args: &[&str]| assert!(Command::new("git").args(args).current_dir(repo.path()).status().unwrap().success());
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(repo.path().join("README"), "x\n").unwrap();
    git(&["add", "."]);
    git(&["-c", "user.name=a", "-c", "user.email=a@b", "commit", "-qm", "x"]);
    let project = format!("e2e-fork-{}", std::process::id());
    ok(&["repo", "add", &project, &repo.path().display().to_string()]);
    {
        let b = Berth::new(&["--project", &project, "--branch", "main"]);
        b.sh_ok("echo wip > work.txt");
        let fork = Berth(ok(&["fork", "--branch", "try-2", &b.0]));
        assert_eq!(fork.sh_ok("git branch --show-current; cat work.txt"), "try-2\nwip");
        assert_eq!(b.sh_ok("git branch --show-current"), "main");
    }
    ok(&["repo", "rm", &project]);
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn snapshots_go_with_their_berth() {
    let b = Berth::new(&[]);
    ok(&["snapshot", "create", &b.0, "one"]);
    assert!(!berth(&["snapshot", "create", &b.0, "one"]).status.success(), "a name was taken twice");
    assert!(!berth(&["snapshot", "create", &b.0, "../x"]).status.success());
    assert!(!berth(&["restore", &b.0, "missing"]).status.success());
    ok(&["snapshot", "rm", &b.0, "one"]);
    assert_eq!(ok(&["snapshot", "ls", "--json", &b.0]), "[]");
    // A fork with a branch needs a project to have branches.
    assert!(!berth(&["fork", "--branch", "x", &b.0]).status.success());
}
