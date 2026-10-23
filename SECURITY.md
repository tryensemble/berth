# Security

berth exists to keep code in one berth away from other berths and from the
machine. A way around that is a security issue. That includes reaching another
berth or the host, gaining privileges on the host, or reading or writing files
outside the berth.

## Reporting

Report it privately through
[GitHub's security advisories](https://github.com/tryensemble/berth/security/advisories/new),
not in a public issue. Include what you ran and what you reached. We'll reply
within a few working days.

## What berth defends, and what it doesn't

Each berth has its own user, mount, PID, network, IPC, UTS and cgroup
namespaces, and a root that's an unused uid range on the host, which the
daemon keeps every host account and subordinate range out of. A seccomp
filter refuses the kernel's riskiest calls, `/proc` and `/sys` hide the host,
and no device node outside the berth's own `/dev` opens. Berths can't reach
each other, the host, private networks or cloud metadata. The daemon never
runs git as root, and unpacks images in a confined process of their own. The
end-to-end tests in [tests/e2e](tests/e2e) try these walls from inside a berth.

Known limits, which aren't bugs to report:

- By default berths share the host's kernel, so a kernel bug reachable from
  inside a berth can break the wall; keep the kernel patched. A berth made
  `--strict` runs its commands under gVisor, a user-space kernel, which closes
  that gap for code nobody vouches for, at some cost to speed.
- `/proc` and `/sys` masks are berth root's own mounts, left unlocked so
  Docker, bubblewrap and gVisor can mount their own `/proc`: berth root can
  see past them, but the kernel's own checks still keep it from anything
  that matters (see [the architecture](docs/ARCHITECTURE.md#isolation-of-a-berth)).
- The daemon checks `/etc/passwd`, `/etc/group`, `/etc/subuid` and
  `/etc/subgid` against berth's ids (868928 up), but can't see ids handed out
  elsewhere: directory-service accounts (LDAP, SSSD), or ranges
  systemd-nspawn, Incus or LXD pick for themselves. Keep those out of the
  range.
- Members of the `berth` group control every berth on the machine, and can
  reach any berth's ports through its SOCKS socket; the group is for
  operators, not for the agents.
- A published port (`berth publish`) is reachable by anyone who can reach the
  host address it's published on: 127.0.0.1 by default.
- A berth can use its share of the machine (CPU, memory, processes, as
  limited) and the network to the internet. It can't take more, but it can
  use what it has for anything.
