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

## Status

berth is a prototype. These are known and planned, not bugs to report:

- There's no seccomp allowlist or Landlock policy yet (milestone 2). Berths
  rely on namespaces, an unprivileged user namespace, cgroups and the network
  rules alone.
- Berths share the host's kernel. A kernel bug reachable from inside a berth
  can break the wall. Strict mode (gVisor) is planned for code nobody vouches
  for.
