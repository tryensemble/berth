//! A berth's network: its own namespace with a veth into the daemon's bridge,
//! NAT to the internet, and rules that keep it away from the host, other
//! berths and private networks.
//!
//! The prototype drives `ip`, `bridge`, `nft` and `iptables`; netlink comes
//! later.

use std::net::Ipv4Addr;

use crate::sys::run;

pub const BRIDGE: &str = "berth0";
pub const GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 200, 0, 1);

const RULES: &str = r#"
table inet berth {
  # Host ports berths may reach on their gateway (a secrets proxy, say).
  set host_ports { type inet_service; }

  chain input {
    type filter hook input priority -10; policy accept;
    iifname "berth0" ct state established,related accept
    iifname "berth0" meta l4proto icmp accept
    iifname "berth0" tcp dport @host_ports accept
    iifname "berth0" drop
  }

  chain forward {
    type filter hook forward priority -10; policy accept;
    iifname "berth0" oifname "berth0" drop
    iifname "berth0" ip daddr { 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 100.64.0.0/10, 169.254.0.0/16, 127.0.0.0/8 } drop
    iifname "berth0" meta nfproto ipv6 drop
  }

  chain postrouting {
    type nat hook postrouting priority 100; policy accept;
    ip saddr 10.200.0.0/16 oifname != "berth0" masquerade
  }
}
"#;

/// Sets up the bridge and the rules; safe to run again.
pub fn setup_host() -> anyhow::Result<()> {
    if run("ip", &["link", "show", BRIDGE]).is_err() {
        run("ip", &["link", "add", BRIDGE, "type", "bridge"])?;
        run("ip", &["addr", "add", &format!("{GATEWAY}/16"), "dev", BRIDGE])?;
    }
    run("ip", &["link", "set", BRIDGE, "up"])?;
    std::fs::write("/proc/sys/net/ipv4/ip_forward", "1")?;

    let _ = run("nft", &["delete", "table", "inet", "berth"]);
    let mut nft = std::process::Command::new("nft")
        .args(["-f", "-"])
        .stdin(std::process::Stdio::piped())
        .spawn()?;
    use std::io::Write;
    nft.stdin.take().unwrap().write_all(RULES.as_bytes())?;
    if !nft.wait()?.success() {
        anyhow::bail!("loading the nftables rules failed");
    }

    // Docker (and ufw) drop forwarded traffic by default; a drop anywhere wins,
    // so berths' traffic is accepted there explicitly. Our own drops above
    // still apply.
    let chain = if run("iptables", &["-L", "DOCKER-USER", "-n"]).is_ok() {
        Some("DOCKER-USER")
    } else if run("iptables", &["-S", "FORWARD"]).map(|s| s.contains("-P FORWARD DROP")).unwrap_or(false) {
        Some("FORWARD")
    } else {
        None
    };
    if let Some(chain) = chain {
        for rule in [
            vec!["-i", BRIDGE, "-j", "ACCEPT"],
            vec!["-o", BRIDGE, "-m", "conntrack", "--ctstate", "RELATED,ESTABLISHED", "-j", "ACCEPT"],
        ] {
            let mut check = vec!["-C", chain];
            check.extend(&rule);
            if run("iptables", &check).is_err() {
                let mut insert = vec!["-I", chain];
                insert.extend(&rule);
                run("iptables", &insert)?;
            }
        }
    }
    Ok(())
}

pub fn address(n: u32) -> Ipv4Addr {
    Ipv4Addr::from(u32::from(Ipv4Addr::new(10, 200, 0, 0)) + n)
}

pub fn host_veth(n: u32) -> String {
    format!("bh{n}")
}

/// Gives the berth whose PID 1 is pid its veth, address and route.
pub fn attach(n: u32, pid: i32) -> anyhow::Result<()> {
    let host = host_veth(n);
    let peer = format!("bp{n}");
    let _ = run("ip", &["link", "del", &host]);
    run("ip", &["link", "add", &host, "type", "veth", "peer", "name", &peer])?;
    run("ip", &["link", "set", &peer, "netns", &pid.to_string()])?;
    run("ip", &["link", "set", &host, "master", BRIDGE])?;
    run("ip", &["link", "set", &host, "up"])?;
    run("bridge", &["link", "set", "dev", &host, "isolated", "on"])?;

    let ns = format!("--net=/proc/{pid}/ns/net");
    let inside = |args: &[&str]| {
        let mut a = vec![ns.as_str(), "ip"];
        a.extend(args);
        run("nsenter", &a)
    };
    inside(&["link", "set", &peer, "name", "eth0"])?;
    inside(&["addr", "add", &format!("{}/16", address(n)), "dev", "eth0"])?;
    inside(&["link", "set", "eth0", "up"])?;
    inside(&["link", "set", "lo", "up"])?;
    inside(&["route", "add", "default", "via", &GATEWAY.to_string()])?;
    Ok(())
}

pub fn detach(n: u32) {
    let _ = run("ip", &["link", "del", &host_veth(n)]);
}

/// Public resolvers for berths: the host's own, minus private addresses berths
/// can't reach.
pub fn resolvers() -> Vec<String> {
    let conf = std::fs::read_to_string("/run/systemd/resolve/resolv.conf")
        .or_else(|_| std::fs::read_to_string("/etc/resolv.conf"))
        .unwrap_or_default();
    let mut out: Vec<String> = conf
        .lines()
        .filter_map(|l| l.strip_prefix("nameserver "))
        .filter_map(|a| a.trim().parse::<Ipv4Addr>().ok())
        .filter(|a| !(a.is_private() || a.is_loopback() || a.is_link_local()))
        .map(|a| a.to_string())
        .collect();
    out.dedup();
    if out.is_empty() {
        out = vec!["1.1.1.1".into(), "8.8.8.8".into()];
    }
    out
}

/// TCP ports something listens on inside the berth whose PID 1 is pid.
pub fn listening_ports(pid: i32) -> Vec<u16> {
    let mut ports = Vec::new();
    for f in ["tcp", "tcp6"] {
        let Ok(table) = std::fs::read_to_string(format!("/proc/{pid}/net/{f}")) else { continue };
        for line in table.lines().skip(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() > 3 && cols[3] == "0A" {
                if let Some(port) = cols[1].rsplit(':').next().and_then(|p| u16::from_str_radix(p, 16).ok()) {
                    ports.push(port);
                }
            }
        }
    }
    ports.sort();
    ports.dedup();
    ports
}
