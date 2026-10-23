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
namespaces, and a root that's an unused uid range on the host. A seccomp filter
refuses the kernel's riskiest calls, and `/proc` and `/sys` hide the host.
Berths can't reach each other, the host, private networks or cloud metadata.
The daemon never runs git or unpacks images as unconfined root. The
end-to-end tests in [tests/e2e](tests/e2e) try these walls from inside a berth.

Known limits, which aren't bugs to report:

- Berths share the host's kernel. A kernel bug reachable from inside a berth
  can break the wall. Keep the kernel patched. Strict mode (gVisor) is for
  code nobody vouches for.
- Members of the `berth` group control every berth on the machine, and can
  reach any berth's ports through its SOCKS socket; the group is for
  operators, not for the agents.
- A published port (`berth publish`) is reachable by anyone who can reach the
  host address it's published on: 127.0.0.1 by default.
- A berth can use its share of the machine (CPU, memory, processes, as
  limited) and the network to the internet. It can't take more, but it can
  use what it has for anything.
