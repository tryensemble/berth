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

const RULES: &str = include_str!("rules.nft");

/// Sets up the bridge and the rules; safe to run again.
pub fn setup_host() -> anyhow::Result<()> {
    if run("ip", &["link", "show", BRIDGE]).is_err() {
        run("ip", &["link", "add", BRIDGE, "type", "bridge"])?;
        run("ip", &["addr", "add", &format!("{GATEWAY}/16"), "dev", BRIDGE])?;
    }
    run("ip", &["link", "set", BRIDGE, "up"])?;
    std::fs::write("/proc/sys/net/ipv4/ip_forward", "1")?;

    let _ = run("nft", &["delete", "table", "inet", "berth"]);
    let mut nft = std::process::Command::new("nft").args(["-f", "-"]).stdin(std::process::Stdio::piped()).spawn()?;
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
    public_resolvers(&conf)
}

/// 100.64.0.0/10, carrier-grade NAT (and Tailscale).
fn is_shared(a: &Ipv4Addr) -> bool {
    let [x, y, ..] = a.octets();
    x == 100 && y & 0xc0 == 64
}

/// The public IPv4 nameservers of a resolv.conf, or Cloudflare's and Google's
/// when it has none.
pub fn public_resolvers(conf: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for a in conf
        .lines()
        .filter_map(|l| l.trim().strip_prefix("nameserver"))
        .filter_map(|a| a.trim().parse::<Ipv4Addr>().ok())
        .filter(|a| !(a.is_private() || a.is_loopback() || a.is_link_local() || is_shared(a)))
    {
        if !out.contains(&a.to_string()) {
            out.push(a.to_string());
        }
    }
    if out.is_empty() {
        out = vec!["1.1.1.1".into(), "8.8.8.8".into()];
    }
    out
}

/// TCP ports something listens on inside the berth whose PID 1 is pid.
pub fn listening_ports(pid: i32) -> Vec<u16> {
    let mut ports = Vec::new();
    for f in ["tcp", "tcp6"] {
        if let Ok(table) = std::fs::read_to_string(format!("/proc/{pid}/net/{f}")) {
            ports.extend(listening_in(&table));
        }
    }
    ports.sort();
    ports.dedup();
    ports
}

/// The local ports of the sockets in LISTEN state in a /proc/net/tcp table.
pub fn listening_in(table: &str) -> Vec<u16> {
    const LISTEN: &str = "0A";
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() > 3 && cols[3] == LISTEN {
                cols[1].rsplit(':').next().and_then(|p| u16::from_str_radix(p, 16).ok())
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_follow_the_berths_number() {
        assert_eq!(address(2), Ipv4Addr::new(10, 200, 0, 2));
        assert_eq!(address(300), Ipv4Addr::new(10, 200, 1, 44));
        assert_eq!(host_veth(7), "bh7");
    }

    #[test]
    fn resolvers_skip_what_berths_cant_reach() {
        let conf = "# systemd\nnameserver 127.0.0.53\nnameserver 10.0.0.2\nnameserver 100.100.100.100\n\
                    nameserver 9.9.9.9\nnameserver 9.9.9.9\nnameserver 2620:fe::fe\nsearch lan\n";
        assert_eq!(public_resolvers(conf), ["9.9.9.9"]);
        assert_eq!(public_resolvers("nameserver 192.168.1.1\n"), ["1.1.1.1", "8.8.8.8"]);
    }

    #[test]
    fn listening_ports_come_from_listen_sockets_only() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:0BB8 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 1 1
   1: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 2 1
   2: 0100007F:0BB8 0100007F:D2F0 01 00000000:00000000 00:00000000 00000000  1000        0 3 1
";
        assert_eq!(listening_in(table), [3000, 8080]);
        let table6 = "  sl  local_address remote_address st
   0: 00000000000000000000000001000000:1538 00000000000000000000000000000000:0000 0A
";
        assert_eq!(listening_in(table6), [5432]);
    }

    #[test]
    fn the_rules_keep_berths_apart() {
        assert!(RULES.contains("table inet berth"));
        assert!(RULES.contains(r#"iifname "berth0" oifname "berth0" drop"#));
        assert!(RULES.contains("169.254.0.0/16"));
    }
}
