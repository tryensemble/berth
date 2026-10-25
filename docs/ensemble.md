# Driving berth from Ensemble

Ensemble gives each thread a machine through its `sandbox.Sandbox` and
`sandbox.Machine` interfaces (`apps/host/internal/sandbox/sandbox.go` in
Ensemble). A berth driver implements them with the `berth` command line, run
on the box over SSH (`ssh box berth ...`) or locally when Ensemble's host is
the box. Every call below is a single command; `--json` output is stable.

A thread's berth is named after the thread: `--name <threadID>`. Every command
that takes a berth's id takes that name too, so the driver never has to keep
ids.

## Sandbox

| Ensemble | berth |
|---|---|
| `Create(ctx, threadID, image, size)` | `berth create --name <threadID> --layer <image's layers> --cpus <size.CPUs> --memory <size.Memory> --host-port <secrets proxy port>` |
| `Open(ctx, threadID)` | `berth ls --json`, find `name == threadID`; `ErrGone` when absent |

An Ensemble image is a list of berth layers (`tools`, `docker`, a project's
own). With `--project` and `--branch` berth checks the project out itself from
its mirror (`berth repo add`); Ensemble can instead keep cloning over HTTP, as
for other non-host machines (`UsesHostFiles` is false).

## Machine

| Ensemble | berth |
|---|---|
| `Command(ctx, spec)` | `berth exec <threadID> --dir /workspace/<spec.Dir> -e K=V... -- <spec.Args>`; add `-t` for a terminal |
| `UsesHostFiles()` | `false` |
| `HostAddress()` | `10.200.0.1`, the berth's gateway; the secrets proxy's port is reachable because of `--host-port` |
| `Destroy(ctx)` | `berth destroy <threadID>` |
| `ListeningPorts(ctx)` | `berth ports <threadID> --json` |
| `SocksProxy(ctx, port)` | locally: `berth socks <threadID> --listen 127.0.0.1:<port>`; remotely: `ssh -N -L 127.0.0.1:<port>:$(berth socks <threadID>) box` (OpenSSH forwards a TCP port to a Unix socket) |

`exec` passes stdin, stdout and stderr through and exits with the command's
code, so `exec.Cmd` works as it does for the other drivers. `-e KEY` without a
value passes on the caller's own `KEY`, which keeps secrets off command lines.

## Optional interfaces

| Ensemble | berth |
|---|---|
| `ImageBuilder.HasImage(name)` | `berth layer ls`, look for `name` |
| `ImageBuilder.BuildImage(ctx, name, prepare)` | `berth create --name build-<name>`, run `prepare` against it as a Machine, then `berth layer commit build-<name> <name>` (the berth becomes the layer) |
| `ImageBuilder.RemoveImagesExcept(prefix, keep)` | `berth layer ls`, `berth layer rm` each other one with the prefix; layers berths still use are refused |
| `Pauser.Paused(threadID)` | `status` is `idle` or `stopped` in `berth ls --json`; it doesn't wake the berth |
| `Pauser.Running(threadID)` | `status` is `running` |
| `Pauser.KeepsPrograms()` | `true`: an idle berth's programs carry on where they were when it wakes. A berth stopped after a long idle time starts its services again (`exec --service`) |
| `PausableMachine.Pause(ctx)` | `berth idle <threadID>` |
| `TrimmableMachine.Trim(ctx)` | `berth trim <threadID>` |
| `Inventory.Machines(ctx)` | `berth ls --json`, the `name` of each berth |
| `Inventory.RemoveMachine(ctx, threadID)` | `berth destroy <threadID>`; a berth that isn't there is an error to ignore |

`TunnelledMachine` and `FolderMachine` don't apply: a berth is on the box,
not on the host's own computer.

## What berth does on its own

- **Idle and back.** A berth unused for `--idle-after` (5 minutes by default)
  is frozen and its memory pushed out; any `exec`, `proxy` or SOCKS connection
  wakes it within milliseconds. After `--stop-after` (12 hours) it's stopped and
  starts again on its next use. Ensemble needn't pause berths itself.
- **Forks.** `berth fork <threadID> --name <newThreadID> --branch <branch>`
  makes a new thread's berth from another's files, services and Docker state,
  in a fraction of a second on XFS.
- **Docker.** A berth with the Docker layer has its own Docker, pulling through
  its tenant's registry mirror (`--tenant`, one per Ensemble account, say).
- **A screen.** A berth with the desktop layer has a screen with Chrome:
  `berth screenshot <threadID> <file>` saves it, `berth desktop <threadID>`
  serves a page to watch it, and Chrome's DevTools are on the berth's port
  9222 for the thread's browser tools.
- **Strict mode.** `berth create --strict` (with the gvisor layer) runs the
  thread's commands under gVisor, for code nobody vouches for; the driver
  needn't do anything else differently.

## Errors

`berth` exits 1 and prints `berth: <message>` on stderr. `no berth <name>` means
the berth is gone (`ErrGone`); `connecting to berthd` means the daemon isn't
answering (`ErrUnavailable`).
