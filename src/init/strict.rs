//! Strict mode: running a berth's commands under gVisor (runsc).
//!
//! The berth is an ordinary berth — its namespaces, overlay root, network,
//! cgroup, ports and services are all as usual. Strict mode changes only how a
//! command runs: instead of exec-ing it directly, init runs it under `runsc`,
//! which puts gVisor's user-space kernel between the command and the host
//! kernel. The command's syscalls are served by the gVisor sentry, so a bug in
//! the host kernel reachable from inside the berth no longer is.
//!
//! runsc shares the berth's network (`--network=host`, in the berth's own
//! netns), so a server under gVisor listens on a real socket the berth's
//! `ports`, `proxy` and SOCKS already reach. Each command is its own small
//! sandbox over a shared read bind of the berth's root.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use serde_json::json;

use super::exec::Resolved;
use crate::sys;

/// runsc, from the gVisor layer.
const RUNSC: &str = "/usr/local/bin/runsc";
/// Under /run (tmpfs): the shared rootfs bind, per-command bundles, and
/// runsc's own state, none of which should outlive the berth.
const DIR: &str = "/run/berth-strict";

fn rootfs() -> PathBuf {
    Path::new(DIR).join("rootfs")
}

fn state() -> PathBuf {
    Path::new(DIR).join("state")
}

/// Whether this berth runs commands under gVisor, and runsc is there to do it.
pub fn available() -> bool {
    Path::new(RUNSC).exists()
}

/// Sets up what every strict command shares: one read-write bind of the
/// berth's root to run sandboxes over, so no command needs its own mount.
pub fn setup() -> anyhow::Result<()> {
    if !available() {
        bail!("this berth is strict but has no {RUNSC}; add the gvisor layer");
    }
    // Bundles under here hold each command's environment (its secrets), so the
    // tree is the berth-root's alone, not readable by the agent or anyone else.
    fs::create_dir_all(DIR)?;
    fs::set_permissions(DIR, fs::Permissions::from_mode(0o700))?;
    fs::create_dir_all(rootfs())?;
    fs::create_dir_all(state())?;
    use libc::{MS_BIND, MS_REC};
    sys::mount(Some("/"), &rootfs(), None, MS_BIND | MS_REC, None)
        .context("binding the berth's root for strict mode")?;
    Ok(())
}

/// A `runsc` command that runs r's command under gVisor, wired like any other:
/// run_pipes and run_tty treat it as the command. The bundle and sandbox are
/// cleaned up when it exits.
pub fn command(r: &Resolved) -> anyhow::Result<Command> {
    let id = format!("berth-{:08x}", rand());
    let bundle = Path::new(DIR).join(&id);
    fs::create_dir_all(&bundle)?;
    fs::set_permissions(&bundle, fs::Permissions::from_mode(0o700))?;
    write_config(&bundle, r)?;

    // A wrapper so the sandbox and bundle are removed however runsc exits,
    // while its exit code is still the caller's. runsc is the child, so the
    // existing kill-the-process-group on hangup stops the sandbox too.
    //   --network=host    the berth's own netns, so its ports and rules hold
    //   --overlay2=none   write straight through: the berth's root is already
    //                     an overlay, so gVisor's own (discarded) one would
    //                     only lose the command's writes
    //   --file-access=shared  concurrent sandboxes over the root see each
    //                     other's changes, as processes in one berth should
    //   --ignore-cgroups  the berth's cgroup is already its limit
    let state = state();
    let flags = "--network=host --overlay2=none --file-access=shared --ignore-cgroups";
    let run = format!(
        "{RUNSC} {flags} --root {state} run -bundle {bundle} {id}; s=$?; \
         {RUNSC} --root {state} delete -force {id} 2>/dev/null; rm -rf {bundle}; exit $s",
        state = state.display(),
        bundle = bundle.display(),
    );
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(run);
    Ok(cmd)
}

/// Writes the bundle's OCI config: gVisor's default spec with the command,
/// user, directory, environment and root filled in, the network namespace
/// dropped so the sandbox shares the berth's, and full capabilities for root
/// so sudo and installs work. Built here rather than by `runsc spec`, because
/// init reaps every child and would reap that subprocess before it's waited on.
fn write_config(bundle: &Path, r: &Resolved) -> anyhow::Result<()> {
    // Root in the berth already maps to an unprivileged host id; these let it
    // act as root inside the sandbox (apt, sudo). Non-root gets none, as usual.
    let caps: Vec<&str> = if r.uid == 0 {
        vec![
            "CAP_CHOWN",
            "CAP_DAC_OVERRIDE",
            "CAP_DAC_READ_SEARCH",
            "CAP_FOWNER",
            "CAP_FSETID",
            "CAP_KILL",
            "CAP_SETGID",
            "CAP_SETUID",
            "CAP_SETPCAP",
            "CAP_NET_BIND_SERVICE",
            "CAP_NET_RAW",
            "CAP_SYS_CHROOT",
            "CAP_MKNOD",
            "CAP_AUDIT_WRITE",
            "CAP_SETFCAP",
            "CAP_SYS_ADMIN",
            "CAP_SYS_PTRACE",
        ]
    } else {
        vec![]
    };
    let config = json!({
        "ociVersion": "1.0.0",
        "process": {
            "terminal": false,
            "user": { "uid": r.uid, "gid": r.gid, "additionalGids": r.groups },
            "args": r.argv,
            "env": r.env,
            "cwd": r.cwd,
            "capabilities": { "bounding": caps, "effective": caps, "inheritable": caps, "permitted": caps },
        },
        "root": { "path": rootfs().display().to_string(), "readonly": false },
        "hostname": hostname(),
        "mounts": [
            { "destination": "/proc", "type": "proc", "source": "proc" },
            { "destination": "/dev", "type": "tmpfs", "source": "tmpfs" },
            { "destination": "/sys", "type": "sysfs", "source": "sysfs", "options": ["nosuid", "noexec", "nodev", "ro"] },
        ],
        // No network namespace: the sandbox shares the berth's, so its ports
        // are the berth's and its network rules still hold.
        "linux": { "namespaces": [{ "type": "pid" }, { "type": "ipc" }, { "type": "uts" }, { "type": "mount" }] },
    });
    // 0600: the config embeds the command's environment, secrets and all.
    let path = bundle.join("config.json");
    fs::write(&path, serde_json::to_vec(&config)?)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// The berth's hostname, for the sandbox to share.
fn hostname() -> String {
    fs::read_to_string("/etc/hostname").map(|h| h.trim().to_string()).unwrap_or_else(|_| "berth".into())
}

/// A small random number/// A small random number, for a unique sandbox id.
fn rand() -> u32 {
    let mut b = [0u8; 4];
    use std::io::Read;
    fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).map(|()| u32::from_ne_bytes(b)).unwrap_or(0)
}
