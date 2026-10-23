//! Strict mode: a berth whose commands run under gVisor (runsc). It's an
//! ordinary berth in every other way, so only the kernel the commands see
//! changes.
//!
//! These need the gvisor layer, which `strict_berth` builds once.

use std::sync::OnceLock;

use crate::{Berth, berth, get, ok};

/// A gVisor layer on top of the test layer, built once.
fn gvisor_layer() -> &'static str {
    static CELL: OnceLock<Result<String, String>> = OnceLock::new();
    let base = crate::layer();
    crate::built(
        &CELL,
        "set -e
        apt-get update -qq
        apt-get install -y -qq --no-install-recommends bzip2 ca-certificates curl >/dev/null
        rel=$(curl -fsSL https://api.github.com/repos/google/gvisor/releases/latest | sed -n 's/.*\"tag_name\": *\"\\(release-[0-9.]*\\)\".*/\\1/p' | head -1)
        cd /tmp
        curl -fsSLO https://github.com/google/gvisor/releases/download/$rel/gvisor-x86_64.tar.bz2
        curl -fsSLO https://github.com/google/gvisor/releases/download/$rel/SHA512SUMS
        grep gvisor-x86_64.tar.bz2 SHA512SUMS | sha512sum -c -
        mkdir -p /opt/gvisor && tar xjf gvisor-x86_64.tar.bz2 -C /opt/gvisor
        ln -sf /opt/gvisor/runsc /usr/local/bin/runsc
        rm -rf /tmp/* /var/lib/apt/lists/*",
        &[base],
    )
}

fn strict_berth() -> Berth {
    Berth::new_with_layers(&[crate::layer(), gvisor_layer()], &["--strict", "--memory", "2G"])
}

#[test]
#[ignore = "needs a running berthd with the gvisor layer; run with --ignored"]
fn commands_run_under_the_gvisor_kernel() {
    let strict = strict_berth();
    // gVisor presents its own kernel version, not the host's.
    assert!(strict.sh_ok("uname -r").ends_with("-gvisor"));
    assert!(strict.sh_ok("cat /proc/version").contains("gvisor"));
    // A normal berth sees the real host kernel.
    let normal = Berth::new(&[]);
    assert!(!normal.sh_ok("uname -r").contains("gvisor"));
    assert_ne!(strict.sh_ok("uname -r"), normal.sh_ok("uname -r"));
}

#[test]
#[ignore = "needs a running berthd with the gvisor layer; run with --ignored"]
fn a_strict_berth_is_still_an_ordinary_berth() {
    let b = strict_berth();
    // Identity, workspace, exit codes, stdin, environment, a terminal.
    assert_eq!(b.sh_ok("id -un; pwd"), "agent\n/workspace");
    assert_eq!(b.sh("exit 9").status.code(), Some(9));
    b.sh_ok("echo kept > /workspace/f");
    assert_eq!(b.sh_ok("cat /workspace/f"), "kept");
    let out = berth(&["exec", "-e", "GREET=hi", &b.0, "--", "sh", "-c", "echo $GREET"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi\n");
    assert!(b.sh("test -t 0 < /dev/null").status.code() == Some(1));
}

#[test]
#[ignore = "needs a running berthd with the gvisor layer; run with --ignored"]
fn a_server_under_gvisor_is_reachable_through_the_berths_ports() {
    let b = strict_berth();
    // The server (perl, in the base image) runs under gVisor; the berth's own
    // ports and proxy reach it over the berth's network.
    b.serve("127.0.0.1", 3000, "served under gvisor");
    assert_eq!(ok(&["ports", &b.0]), "3000");
    assert_eq!(get(&b.0, 3000), "served under gvisor");
    // The process really is under the gVisor kernel.
    assert!(b.sh_ok("uname -r").ends_with("-gvisor"));
}

#[test]
#[ignore = "needs a running berthd with the gvisor layer; run with --ignored"]
fn strict_berths_still_cant_reach_each_other_or_the_host() {
    let (a, b) = (strict_berth(), strict_berth());
    b.serve("0.0.0.0", 8080, "open");
    let b_ip = b.info()["ip"].as_str().unwrap().to_string();
    for target in [format!("{b_ip}/8080"), "10.200.0.1/22".into(), "169.254.169.254/80".into()] {
        assert!(!a.sh(&format!("timeout 3 bash -c 'echo > /dev/tcp/{target}'")).status.success(), "reached {target}");
    }
    assert!(a.sh("timeout 5 curl -sSf https://example.com -o /dev/null").status.success(), "no internet");
}

#[test]
#[ignore = "needs a running berthd with the gvisor layer; run with --ignored"]
fn a_strict_berth_idles_and_wakes() {
    let b = strict_berth();
    b.serve("127.0.0.1", 3000, "awake");
    ok(&["idle", &b.0]);
    assert_eq!(b.status(), "idle");
    assert_eq!(get(&b.0, 3000), "awake");
    assert_eq!(b.status(), "running");
}

#[test]
#[ignore = "needs a running berthd with the gvisor layer; run with --ignored"]
fn strict_needs_the_gvisor_layer() {
    // A strict berth with no runsc fails to start, rather than running
    // commands unprotected.
    let out = berth(&["create", "--layer", crate::layer(), "--strict"]);
    assert!(!out.status.success());
    let msg = String::from_utf8_lossy(&out.stderr);
    assert!(msg.contains("gvisor") || msg.contains("runsc"), "{msg}");
}
