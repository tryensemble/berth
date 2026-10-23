//! What a driver like Ensemble's builds on: berths found by name, host ports
//! for a secrets proxy, SOCKS on a TCP port, images built interactively, and
//! trimming.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::{Berth, berth, layer, ok, wait_for};

/// A port this test process can use on the host.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn berths_are_found_by_name() {
    let name = format!("thread-{}", std::process::id());
    let b = Berth::new(&["--name", &name]);
    assert_eq!(b.sh_ok("hostname"), format!("berth-{}", b.0));
    assert_eq!(ok(&["exec", &name, "--", "hostname"]), format!("berth-{}", b.0));
    assert_eq!(b.info()["name"], name.as_str());
    let taken = berth(&["create", "--layer", layer(), "--name", &name]);
    assert!(!taken.status.success(), "two berths got one name");
    ok(&["idle", &name]);
    ok(&["start", &name]);
    ok(&["destroy", &name]);
    assert!(!berth(&["exec", &b.0, "--", "true"]).status.success());
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn a_berth_reaches_only_the_host_ports_it_was_given() {
    // A stand-in for a secrets proxy, on every host address.
    let proxy = TcpListener::bind("0.0.0.0:0").unwrap();
    let port = proxy.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut c in proxy.incoming().flatten() {
            let _ = c.write_all(b"proxy\n");
        }
    });
    let allowed = Berth::new(&["--host-port", &port.to_string()]);
    let other = Berth::new(&[]);
    let probe = format!("timeout 3 bash -c 'exec 3<>/dev/tcp/10.200.0.1/{port}; head -1 <&3'");
    assert_eq!(allowed.sh_ok(&probe), "proxy");
    assert!(!other.sh(&probe).status.success(), "a berth reached a host port it wasn't given");
    // Its other host ports stay shut.
    assert!(!allowed.sh("timeout 3 bash -c 'echo > /dev/tcp/10.200.0.1/22'").status.success());
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn socks_serves_on_a_tcp_port_for_ensembles_previews() {
    let b = Berth::new(&[]);
    b.serve("127.0.0.1", 3000, "preview");
    let port = free_port();
    let mut relay = Command::new(env!("CARGO_BIN_EXE_berth"))
        .args(["socks", &b.0, "--listen", &format!("127.0.0.1:{port}")])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // It says when it's listening.
    let mut line = String::new();
    BufReader::new(relay.stderr.take().unwrap()).read_line(&mut line).unwrap();
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    s.write_all(&[5, 1, 0]).unwrap();
    let mut greeting = [0u8; 2];
    s.read_exact(&mut greeting).unwrap();
    s.write_all(&[5, 1, 0, 3, 9]).unwrap();
    s.write_all(b"localhost").unwrap();
    s.write_all(&3000u16.to_be_bytes()).unwrap();
    let mut reply = [0u8; 10];
    s.read_exact(&mut reply).unwrap();
    assert_eq!(reply[1], 0, "the SOCKS server refused");
    s.write_all(b"GET / HTTP/1.0\r\n\r\n").unwrap();
    let mut body = String::new();
    s.read_to_string(&mut body).unwrap();
    assert_eq!(body.lines().last(), Some("preview"));
    relay.kill().unwrap();
    let _ = relay.wait();
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn an_image_is_a_berth_prepared_by_hand() {
    let name = format!("e2e-commit-{}", std::process::id());
    let builder = Berth::new(&[]);
    builder.root("echo prepared > /opt/marker");
    ok(&["layer", "commit", &builder.0, &name]);
    // The builder became the layer.
    wait_for(|| !ok(&["ls"]).contains(&builder.0));
    {
        let b = Berth::new_with_layers(&[layer(), &name], &[]);
        assert_eq!(b.sh_ok("cat /opt/marker"), "prepared");
    }
    ok(&["layer", "rm", &name]);
}

#[test]
#[ignore = "needs a running berthd; run with --ignored"]
fn trimming_hands_back_cache_and_keeps_programs() {
    let b = Berth::new(&[]);
    b.serve("127.0.0.1", 3000, "still here");
    // Fill the page cache with a file the berth read.
    b.sh_ok("head -c 64M /dev/zero > /workspace/blob && cat /workspace/blob > /dev/null");
    let v: serde_json::Value = serde_json::from_str(&{
        let out = berth(&["trim", &b.0]);
        assert!(out.status.success());
        // trim prints nothing; read the numbers from the API through ls.
        ok(&["ls", "--json"])
    })
    .unwrap();
    let mem = v.as_array().unwrap().iter().find(|x| x["id"] == b.0.as_str()).unwrap()["memory"].as_u64().unwrap();
    assert!(mem < 48 << 20, "after trimming the berth still holds {} MB", mem >> 20);
    assert_eq!(crate::get(&b.0, 3000), "still here");
    assert_eq!(b.status(), "running");
}
