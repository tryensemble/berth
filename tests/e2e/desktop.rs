//! A berth's screen: the desktop layer's virtual display with Chrome on it,
//! screenshot from outside and watched through the berth's ports.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use crate::{Berth, berth, ok, wait_up_to};

/// The desktop layer, as shipped, on top of the test layer.
fn desktop_layer() -> &'static str {
    static CELL: OnceLock<Result<String, String>> = OnceLock::new();
    let base = crate::layer();
    crate::built(&CELL, include_str!("../../layers/desktop.sh"), &[base])
}

/// What GET path on port in berth id answers, headers and all.
fn fetch(id: &str, port: u16, path: &str) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_berth"))
        .args(["proxy", id, &port.to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _ = child.stdin.take().unwrap().write_all(format!("GET {path} HTTP/1.0\r\n\r\n").as_bytes());
    String::from_utf8_lossy(&child.wait_with_output().unwrap().stdout).into_owned()
}

/// The width and height a PNG says it has.
fn png_size(png: &[u8]) -> (u32, u32) {
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"), "not a PNG");
    let be = |b: &[u8]| u32::from_be_bytes(b.try_into().unwrap());
    (be(&png[16..20]), be(&png[20..24]))
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn a_berth_has_a_screen_chrome_can_draw_on() {
    let b = Berth::new_with_layers(&[crate::layer(), desktop_layer()], &["--memory", "2G"]);
    let dir = tempfile::tempdir().unwrap();
    let shot = dir.path().join("shot.png");
    let shot_arg = shot.display().to_string();

    // Chrome starts a screen when there's none yet, and opens a window on
    // it, with DevTools for tools that drive it.
    let page = "data:text/html,<title>berth-e2e</title><h1>hello</h1>";
    ok(&["exec", "-d", "--name", "chrome", &b.0, "--", "berth-browser", page]);
    wait_up_to(Duration::from_secs(30), || b.sh("DISPLAY=:0 xdotool search --name berth-e2e").status.success());
    assert!(b.sh_ok("curl -s 127.0.0.1:9222/json/version").contains("Chrome/"));

    // A screenshot finds it, and leaves the berth's screen service waiting
    // behind it.
    ok(&["screenshot", &b.0, &shot_arg]);
    assert_eq!(png_size(&std::fs::read(&shot).unwrap()), (1920, 1080));
    assert!(ok(&["service", "ls", &b.0]).contains("desktop"));
    assert_eq!(b.sh_ok("pgrep -cx Xvfb").trim(), "1", "one screen at a time");

    // The page to watch it is served through the berth's ports.
    assert!(fetch(&b.0, 6080, "/vnc.html").contains("noVNC"));

    // The screen sleeps and wakes with the berth.
    ok(&["idle", &b.0]);
    assert_eq!(b.status(), "idle");
    ok(&["screenshot", &b.0, &shot_arg]);
    assert_eq!(png_size(&std::fs::read(&shot).unwrap()), (1920, 1080));
    assert!(b.sh("DISPLAY=:0 xdotool search --name berth-e2e").status.success(), "Chrome is still there");

    // When the screen Chrome started stops, the service's takes over.
    b.sh_ok("pkill -x Xvfb");
    ok(&["screenshot", &b.0, &shot_arg]);
    assert_eq!(png_size(&std::fs::read(&shot).unwrap()), (1920, 1080));
    assert_eq!(b.sh_ok("pgrep -cx Xvfb").trim(), "1");
    wait_up_to(Duration::from_secs(10), || fetch(&b.0, 6080, "/vnc.html").contains("noVNC"));
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn a_berth_without_the_desktop_layer_says_so() {
    let b = Berth::new(&[]);
    let out = berth(&["screenshot", &b.0, "/tmp/berth-e2e-none.png"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("desktop layer"));
}
