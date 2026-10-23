# Architecture

`berth` turns one Linux computer into a home for many agent threads. Each
thread gets a **berth**: its own checkout, processes, ports, Docker, CPU and
memory, fully isolated from every other berth, while the operating system, the
tools (Claude Code, Codex, a browser, mise runtimes) and the download caches
are installed once and shared.

A berth is not a virtual machine. It reserves nothing: no fixed memory, no
disk image, no kernel of its own. It uses what its programs use, exactly as if
they ran in a worktree on the same server, and nothing more. An idle berth
costs no CPU and next to no memory; it comes back the moment it's used.

It knows nothing about any particular agent app. Ensemble is its first user;
anything that can run a command over SSH can be another.

## What it must do

- Be light: a berth costs only what its programs use. Dozens to hundreds of
  berths per computer, most of them idle at any time.
- Idle berths use no CPU and almost no memory, and resume immediately.
- Full isolation: a berth can't read, change, slow down or reach another
  berth or the computer, even when its agent is prompt-injected and actively
  tries. Berths of different customers may share a computer.
- Every berth can run `docker compose`; two berths can both serve `:3000` and
  both run Postgres on `:5432`.
- Agents run inside their berth with their ordinary file and shell tools.
- One static binary, no Docker on the host, Ubuntu 24.04 servers and boat.dev
  machines.

## How strong the wall is

Berths share the computer's Linux kernel; that's what makes them light. The
wall around a berth is made of the kernel's own isolation (namespaces, cgroups,
seccomp, Landlock), the same family of walls containers use, built tighter:
nothing in a berth is real root, it sees only its own files, processes and
network, and the kernel calls it may make are narrowed.

The one thing a shared kernel can't rule out is a bug in the kernel itself
that's reachable from inside. Virtual machines rule that out, at the cost of a
kernel and reserved memory per machine, which is what we're getting away from.
berth's answer, in order:

1. Make the reachable kernel small: a seccomp filter that refuses the
   kernel's riskiest calls, no real root, no host devices, no kernel modules,
   no BPF, no io_uring.
2. Keep the computer's kernel patched (Ubuntu's livepatch where available).
3. A **strict** mode per berth for when that isn't enough: the same berth, run
   under gVisor, a user-space kernel that sits between the berth's programs and
   the real kernel. It still reserves nothing and starts in milliseconds; it
   costs some speed on file-heavy work (installs, builds). It's an option per
   berth, behind the `Sandbox` trait, not part of the first prototype.

## Processes

One binary, three roles:

| Role | Runs as | What it does |
|---|---|---|
| `berth` | the caller | CLI; talks to the daemon's socket |
| `berthd` | root, systemd | the daemon: berths, images, the API, port proxying |
| `berth-init` | root of a berth's user namespace | PID 1 of a berth; starts, reaps and keeps its processes |

### Why root

The daemon runs as root, because building a berth needs mounts, veth pairs,
nftables and cgroups, and because Ubuntu 24.04's AppArmor
(`kernel.apparmor_restrict_unprivileged_userns=1`) only lets privileged
processes create user namespaces with full rights. Nothing in a berth runs as
real root: every berth is a user namespace whose ids map to an unprivileged
host range of its own (65 536 ids per berth). Root inside a berth can
`apt install`; outside, it is nobody.

Only members of the `berth` group can open the daemon's socket, and the API
offers berth operations only, never "run this as root on the host".

### Why a per-berth init

Each berth's first process is `berth-init`, PID 1 of the berth's PID
namespace. Every command run in the berth is started by it, not by the daemon:

- processes are children of init, so a detached dev server survives a daemon
  restart or upgrade;
- init is already inside every namespace, so nothing has to `setns` into a
  berth from a multithreaded daemon (the hard part of runc's design);
- init reaps zombies and owns the berth's pseudo-terminals;
- it holds the record of the berth's **services** (below) and starts them
  again after a berth was stopped.

The daemon talks to init over a Unix socket in the berth's run directory.
Output of detached processes goes to log files on disk, so the daemon can
reattach to them after a restart.

## Isolation of a berth

Built in Rust by `berthd` itself (no bubblewrap, no OCI runtime), borrowing
from youki's crates and Codex's Linux sandbox:

- **Namespaces**: user, mount, PID, network, IPC, UTS and cgroup.
- **cgroup v2**, one per berth, delegated to the berth so Docker's containers
  count against it: `cpu.max`, `memory.max`, `pids.max`. The berth owns its
  cgroup's directory and process lists, not its limit files: it can arrange
  its processes below its limits, never raise them. Freezing a berth is
  `cgroup.freeze`, killing it `cgroup.kill`.
- **seccomp**: init puts itself and everything it starts under a filter that
  refuses the calls a berth has no use for and that reach the most kernel
  code: module and kernel loading, BPF, `perf_event_open`, io_uring,
  `userfaultfd`, the keyring, file handles, clock, swap and quota changes,
  x32 and other architectures' calls. It's a denylist, not Docker's
  allowlist, on purpose: a berth's root must be able to run Docker and nested
  sandboxes (mount, unshare, setns, namespaced clone), which is most of what
  an allowlist would remove; what's left beyond the denylist is guarded by
  the user namespace. The filter is classic BPF built by berth itself, so the
  binary stays static.
- **Masked /proc and /sys**: `kcore`, `keys`, `kallsyms`, `timer_list`,
  `sched_debug` and `sysrq-trigger` read as empty, `/sys/firmware` and
  `/sys/kernel/security` are empty, and `/proc/sys` is read-only except the
  berth's own network's tunables.
- **No links followed**: a stopped berth's files are its own, so when init
  builds its root again, and when the daemon makes a layer of it, every path
  inside is walked without following links, and mount points the berth
  replaced with links are made directories again.
- **Landlock** confines what the daemon does as root with what strangers
  made: unpacking an image runs in a thread that may write only into that
  image's directory, and git fetches into a mirror only. It isn't applied to
  berths themselves: a Landlock domain can't mount, and Docker in a berth
  must, while inside the berth there's nothing but the berth to protect.
- **The daemon never runs strangers' code or parsers as host root**: git
  fetches run as an id of their own that owns only the mirrors, and only over
  git's ordinary transports.
- **Capabilities** are those of root in a user namespace, which don't apply to
  anything the berth doesn't own.

`no_new_privs` is **not** set on the whole berth: agents run as an ordinary
user (below) and need `sudo` to reach the berth's root for `apt install`. That
root is still confined by the user namespace and seccomp.

## Files

All state lives under `/var/lib/berth`. That filesystem should be **XFS with
reflinks** (or btrfs): a reflink copy of a file costs no disk until one side
changes it, which is what makes new berths and snapshots instant and free, and
XFS project quotas cap each berth's disk. On ext4 everything still works, with
real copies and no disk caps, and the daemon warns.

A berth only ever takes disk for **what it changed**.

### What a berth sees

```
/                  overlayfs
  lower 1          base image     (Ubuntu 24.04, read-only, shared)
  lower 2          tools layer    (Claude Code, Codex, browser, mise runtimes, docker)
  upper            the berth's own (its apt and global installs, /etc changes, /home)
/workspace         the berth's checkout                      (bind, read-write)
/tmp               tmpfs, size-limited, counted in the berth's memory
/var/lib/docker    the berth's own directory on the host disk (not on overlay)
~/.cache/...       shared caches, see below
```

- **Base image**: any OCI image, pulled and unpacked by the daemon itself (no
  Docker needed), Ubuntu 24.04 by default.
- **Tools layer**: built by running installs in a builder berth and keeping its
  upper directory as a read-only layer (`berth layer build`). Layers are
  versioned; a berth keeps the versions it was created with until it's moved
  to newer ones.
- **The berth's own layer** takes `apt install`, `npm -g`, `mise install` and
  so on. They change that berth only and go when it's destroyed.

Shared layers are ordinary files on the host, so the page cache holds them once
for every berth: a hundred berths running Claude Code load it into memory once.

### Checkouts

The daemon keeps one bare mirror per project (`repos/<project>.git`), fetched
in a helper berth with credentials the caller supplies, so the host needs no
git and never keeps a project's git credentials.

A berth's `/workspace` is its **own clone** whose objects come from the mirror
through git alternates. The mirror is mounted read-only in the berth. The clone
has its own refs, branches and index: a berth can't move another berth's
branches or rewrite the mirror, and its new commits stay in its own object
store until pushed.

Its working files are reflink copies of a checkout the daemon keeps ready per
project and branch, so a new berth's checkout takes no time and no disk until
it changes files.

### Caches

npm/pnpm stores, the cargo registry, mise downloads, pip and apt archives are
each an overlay: the shared cache read-only underneath, the berth's own writes
on top. A berth reuses everything already downloaded; nothing it downloads
changes what other berths see until it is **promoted** into the shared cache,
which the daemon does for content-addressed caches that verify their own
checksums (pnpm, cargo, mise), and otherwise only on an explicit
`berth cache promote`. Caches can be scoped per customer (`--tenant`), so one
customer's downloads never feed another's.

Docker images are shared the same way through a pull-through registry mirror
the daemon runs (one per tenant); every berth's Docker is pointed at it.

## Network

Each berth has its own network namespace with a veth into a bridge the daemon
runs, its own private address, and NAT to the internet through nftables.

**Default rules**:

- the public internet is reachable;
- the host, other berths, the LAN, private ranges (RFC 1918, CGNAT,
  link-local) and cloud metadata (`169.254.169.254`) are not;
- except host ports the caller allows at creation (a secrets proxy, say), which
  are reachable on the berth's gateway address.

A berth's policy can tighten this at creation: a domain allowlist (DNS through
the daemon's resolver, resolved addresses fed into nftables sets) or no
internet at all.

**Reaching a berth's ports.** The daemon opens connections from a thread that
has entered the berth's network namespace, so a dev server bound to
`127.0.0.1` works as well as one on `0.0.0.0`. Two berths both serving `:3000`
never collide because nothing is published on the host by default:

- `ports(id)`: what listens in the berth, read from its socket table;
- `proxy(id, port)`: one connection to that port, streamed over the API socket;
- `socks(id)`: a SOCKS5 server on a Unix socket (`/run/berth/socks/<id>.sock`,
  `root:berth`, 0660) where `localhost` means the berth's loopback; every
  connection is opened from inside the berth, so its network rules hold.
  `ssh -L 127.0.0.1:N:/run/berth/socks/<id>.sock` makes it a local port
  anywhere, and `berth socks ID --listen ADDR` on the box itself;
- `publish(id, port)`: optional, maps a berth port to a host port, kept across
  daemon restarts. Anyone who can reach that host address reaches the port.

A connection to a sleeping berth wakes it.

**Secrets proxies.** At creation the caller can give host ports the berth may
reach on its gateway (`--host-port`): an nftables set of (berth address, port),
so each berth reaches only its own. Environment variables come with each
command (`exec -e HTTPS_PROXY=...`), and a CA certificate can be installed by
the caller as the berth's root. That's enough for Ensemble's secrets proxy
without berth knowing about secrets. Transparent interception (redirecting all
`:443` to a proxy) can come later.

## Docker inside a berth

Every berth can run Docker, its own and visible to it only. `dockerd` comes
from a Docker layer (`layers/docker.sh`) and is started by init the first time
something connects to `/run/docker.sock` in the berth, by socket activation
(init holds the listening socket and hands it to `dockerd -H fd://`), so
berths that never use Docker never pay for it, and the first connection is
served.

- It runs as root of the berth's user namespace, with the `overlay2` driver
  (native overlayfs in user namespaces, kernel 5.11 and up; Ubuntu 24.04 has
  6.8) on `/var/lib/docker`, a directory of the berth's on the host disk.
- Its containers live in the berth's delegated cgroup, so they count against
  the berth's CPU, memory and process limits, and freeze with it.
- Its networks live in the berth's network namespace: a compose service
  publishing `5432` shows up in `ports(id)` and is reachable through `proxy`
  like any other port, and two berths' Postgres never meet.
- Pulls go through the tenant's registry mirror: an ordinary berth made from
  `registry:3` in proxy mode, its port published on the gateway, reachable by
  that tenant's berths only. It's made with the tenant's first berth that has
  Docker. `BERTH_REGISTRY_UPSTREAM` points it elsewhere than Docker Hub (a
  machine whose address Docker Hub rate-limits can use `mirror.gcr.io`).

Docker is the part of a berth that reaches the most kernel surface (mounts,
namespaces, netfilter). Strict mode runs it under gVisor with the rest of the
berth.

## Agents in the berth

Claude Code, Codex and the rest run **inside** the berth, as an ordinary user
(`agent`, uid 1000 in the berth) with passwordless `sudo` to the berth's root.
They aren't root by default because Claude Code refuses to skip its permission
prompts as root. Their file and shell tools work unchanged. They reach the
model APIs through the network like any program, and their credentials come in
through the caller's environment or secrets proxy.

`exec(id, cmd, env, tty)` gives a pseudo-terminal allocated by init when asked,
and streams stdin, stdout and stderr both ways.

## Idle and back

A berth that nobody uses should cost nothing and come back immediately, with
its dev server and database still running. Three states, chosen by the daemon
from how long a berth has been idle (and changeable by the caller):

| State | What happens | Costs while in it | Back in |
|---|---|---|---|
| **Running** | normal | what its programs use | — |
| **Idle** | the berth's cgroup is frozen, then its memory is pushed out to compressed swap (`memory.reclaim`) | no CPU, a few MB of memory | instantly; programs carry on where they were, pages come back as they're touched |
| **Stopped** | every process ends, the overlay is unmounted | no CPU, no memory; disk only for what it changed | a second or two; init starts its **services** again |

- A berth goes idle after a few minutes without commands or connections, and is
  stopped only after a long time idle, when the computer needs the swap, or on
  request.
- Any exec, proxy or SOCKS connection to an idle or stopped berth wakes it.
- **Services** are processes started with `exec --service <name>` (the dev
  server, say) plus Docker containers with a restart policy; they're what comes
  back after a stop.
- Idle berths' memory goes to swap, so the computer needs some, ideally with
  zswap (compressed, in memory) in front of a swap file on local disk. The
  daemon warns when there's none; setting it up is the operator's, since swap
  is the whole machine's. Running berths may not swap: a berth over its memory
  limit fails at once instead of crawling. Swap is allowed while it's idle,
  up to its memory limit, and its pages come back as they're touched.

Programs are not saved to disk mid-run (CRIU): it's fragile with Docker,
networking and terminals, and idle plus services gives the same result
reliably.

**Snapshots** of a berth are reflink copies of its layer, checkout and Docker
data, taken while it's frozen, so they're consistent, instant and free until
something changes. **Restore** puts a snapshot back; **fork** makes a new berth
from one.

## Sizes

Limits per berth are ceilings, not reservations: a berth with a 4 GB limit
that uses 300 MB takes 300 MB. Defaults, all changeable per berth at creation
and later:

| Limit | Default |
|---|---|
| CPU | 2 CPUs (`cpu.max`), equal weight |
| Memory | 4 GB hard (`memory.max`); no swap while running |
| Processes | 4096 |
| Disk | 20 GB of changes (layer + checkout + Docker data) |
| `/tmp` | 1 GB |

A berth's own overhead while running is init (a few MB), a veth pair and an
overlay mount. Addresses come from `10.200.0.0/16`.

## API

The daemon listens on `/run/berth/berth.sock` (group `berth`): HTTP/1.1 with
JSON, and connection upgrades for the streams (exec, proxy), the same style as
Docker's API, so a client in any language is a few hundred lines. The CLI is a
client of it and is what callers use over SSH.

```
berth create   [--name N] [--project P --branch B] [--layer L...] [--cpus 2 --memory 4G ...]
               [--tenant T] [--host-port PORT...] [--idle-after 5m] [--stop-after 12h]   -> id
berth exec     ID [-t] [-e K=V] [--dir D] [-d | --service NAME] -- CMD...
berth ports    ID [--json]
berth proxy    ID PORT                  # stdin/stdout <-> the berth's port
berth socks    ID [--listen ADDR]       # the berth's SOCKS socket path, or serve it on TCP
berth publish  ID PORT [--host-port P] [--address A]      berth unpublish ID PORT
berth idle | stop | start | trim  ID
berth snapshot create|ls|rm ID [NAME]  berth restore ID NAME  berth fork ID [--snapshot S] [--branch B]
berth destroy  ID
berth ls       [--json]
berth repo add|ls|rm           berth layer build|commit|ls|rm      berth pull REF
```

Wherever an ID goes, a berth's name goes too.

`--tenant` groups berths for shared caches and registry mirrors; berths are
isolated from each other whatever their tenant.

### Ensemble on top

A `berth` driver for Ensemble's `sandbox.Machine` stays thin: every method is
one `berth` command, run on the box over SSH. [ensemble.md](ensemble.md) maps
each of them, the optional interfaces too. The driver itself lives in
Ensemble.

## What we take from prior art

- **Codex's Linux sandbox**: seccomp and Landlock in Rust.
- **youki**: namespace, cgroup v2 and mount code in Rust; how a container's
  init is set up.
- **runc**: why entering namespaces from a multithreaded process is hard (our
  answer is the per-berth init).
- **bubblewrap / Anthropic's sandbox-runtime**: the minimal set of mounts and
  masks a sandbox needs; they don't do cgroups, veth networking or Docker, so
  we don't wrap them.
- **gVisor**: strict mode.
- **Dagger's container-use**: a branch per agent with its own environment; we
  keep git out of the agent's way and give it a whole machine instead.
- **Firecracker**: what we're replacing per thread. A berth starts faster than
  its 1.5 s boot and reserves no memory.

## Dependencies

- Linux 6.8 (Ubuntu 24.04), cgroup v2, nftables, swap (set up by the daemon
  when missing), XFS with reflinks for `/var/lib/berth` (recommended).
- Nothing but the `berth` binary (static, musl). Strict mode adds gVisor's
  `runsc`, which the daemon downloads and checks itself.

## Order of work

1. ✅ **Prototype**: `berthd` and `berth` with create, exec (tty, detach),
   ports, proxy, destroy; base image plus tools layer with Claude Code; veth
   networking. Done when two berths each run Claude Code and both serve
   `:3000`, reachable through `berth proxy`, without colliding.
2. ✅ Limits, seccomp and Landlock hardened; idle, stop, wake and services.
3. ✅ Docker in a berth, the registry mirror. Shared package caches and
   reflinked checkouts are still to come.
4. ✅ Snapshots, fork, and what an Ensemble driver needs (names, host ports,
   SOCKS); the driver itself is Ensemble's.
5. Strict mode (gVisor).

## Open questions

- How fast an idle berth's memory comes back from zswap under real use (a
  Next.js dev server, Postgres in Docker), and how much an idle berth really
  keeps.
- Where agents' model API credentials should live: the caller's environment
  (simple) or always behind a secrets proxy (safer against a prompt-injected
  agent reading them).
