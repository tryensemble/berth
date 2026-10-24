//! Images: pulled fresh, unpacked by the confined unpacker, used and removed.

use crate::{Berth, berth, ok};

/// Small, and not on Docker Hub, whose rate limits CI machines run into.
const IMAGE: &str = "mirror.gcr.io/library/alpine:3.20";

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn an_image_pulls_fresh_runs_and_is_removed() {
    // Gone first, so every run pulls and unpacks it again.
    let _ = berth(&["rmi", IMAGE]);
    let root = ok(&["pull", IMAGE]);
    assert!(root.ends_with("mirror.gcr.io_library_alpine_3.20/rootfs"), "{root}");
    assert!(ok(&["images"]).lines().any(|l| l == "mirror.gcr.io_library_alpine_3.20"));

    // Owners, groups and modes arrive as the image has them.
    let b = Berth::new_with_layers(&[], &["--image", IMAGE]);
    let stat = |path: &str| b.root(&format!("stat -c '%u %g %a' {path}"));
    assert_eq!(stat("/etc/shadow"), "0 42 640");
    assert_eq!(stat("/etc/passwd"), "0 0 644");
    assert_eq!(stat("/var/empty"), "0 0 555");
    assert_eq!(stat("/tmp"), "0 0 1777");

    // An image a berth uses stays.
    let out = berth(&["rmi", IMAGE]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("used by"));
    drop(b);
    ok(&["rmi", IMAGE]);
    assert!(!ok(&["images"]).lines().any(|l| l.contains("alpine")));
    assert!(!berth(&["rmi", IMAGE]).status.success(), "removed twice");
}
