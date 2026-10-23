//! Snapshots of a berth, restoring them, and forking new berths from them.
//!
//! A berth's own state is three directories: its overlay's upper layer (what
//! it installed and changed), its workspace and its Docker data. A snapshot
//! copies the three while the berth is frozen, so the copy is consistent, as
//! after a sudden power cut, Docker's databases included. On a filesystem
//! with reflinks (XFS, btrfs) the copies share their data with the original
//! until either side changes it: snapshots and forks are instant and cost no
//! disk. Elsewhere they're real copies.

use std::fs;
use std::os::unix::fs::{PermissionsExt, chown};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use super::berth::{Berth, State, now};
use super::{Daemon, cgroup, layer, valid_name};
use crate::sys::{self, landlock};

/// What a berth's state is made of, beside the berth's directory.
const PARTS: &[&str] = &["upper", "workspace", "docker"];

/// A snapshot's record, kept next to its copies.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Snapshot {
    pub name: String,
    pub created: u64,
    /// The id range its files are owned by: the berth it was taken of.
    pub uid_base: u32,
    /// What the berth was made of, so a fork is made of the same.
    pub image: String,
    pub layers: Vec<String>,
    pub project: Option<String>,
    pub branch: Option<String>,
    pub services: Vec<crate::api::ExecSpec>,
}

fn dir_of(b: &Berth) -> std::path::PathBuf {
    b.dir().join("snapshots")
}

/// Copies the tree src to dst, which mustn't exist: as reflinks where the
/// filesystem has them, keeping owners, modes, links, device nodes (overlay's
/// whiteouts) and extended attributes (overlay's own among them). The copy
/// runs under Landlock, writing only beneath dst's parent.
pub fn copy_tree(src: &Path, dst: &Path) -> anyhow::Result<()> {
    let parent = dst.parent().context("no parent")?;
    let ruleset = landlock::Ruleset::new(&[parent])?;
    if ruleset.is_none() {
        landlock::warn_once();
    }
    let mut cmd = Command::new("cp");
    cmd.args(["-a", "--reflink=auto", "-T", "--"])
        .arg(src)
        .arg(dst)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null());
    unsafe {
        cmd.pre_exec(move || {
            sys::cvt(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0))?;
            if let Some(r) = &ruleset {
                r.restrict_self()?;
            }
            Ok(())
        });
    }
    let out = cmd.output().context("running cp")?;
    if !out.status.success() {
        bail!("copying {}: {}", src.display(), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Whether files under dir can be reflinked: copies are instant and free.
pub fn reflinks_work(dir: &Path) -> bool {
    // FICLONE, from linux/fs.h.
    const FICLONE: libc::c_ulong = 0x4004_9409;
    let (a, b) = (dir.join(".reflink-probe-a"), dir.join(".reflink-probe-b"));
    let result = (|| -> std::io::Result<bool> {
        fs::write(&a, b"probe")?;
        let src = fs::File::open(&a)?;
        let dst = fs::File::create(&b)?;
        use std::os::fd::AsRawFd;
        Ok(unsafe { libc::ioctl(dst.as_raw_fd(), FICLONE as _, src.as_raw_fd()) } == 0)
    })();
    let _ = fs::remove_file(&a);
    let _ = fs::remove_file(&b);
    result.unwrap_or(false)
}

impl Daemon {
    /// Runs f with the berth frozen (if it runs), so its files hold still.
    fn frozen<T>(&self, b: &Berth, f: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
        let thaw = b.state == State::Running && b.alive() && !cgroup::frozen(b);
        if thaw {
            cgroup::freeze(b, true)?;
        }
        let result = f();
        if thaw {
            cgroup::freeze(b, false)?;
        }
        result
    }

    /// Takes a snapshot of the berth called name.
    pub fn snapshot(&self, id: &str, name: &str) -> anyhow::Result<Snapshot> {
        if !valid_name(name) {
            bail!("bad snapshot name {name}");
        }
        let lock = self.lock_of(id);
        let _held = lock.lock().unwrap();
        let b = self.get(id)?;
        let dir = dir_of(&b);
        let target = dir.join(name);
        if target.exists() {
            bail!("berth {id} already has a snapshot {name}");
        }
        fs::create_dir_all(&dir)?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        // Made under a hidden name and renamed, so a failed copy leaves nothing.
        let tmp = dir.join(format!(".{name}.partial"));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir(&tmp)?;
        let snap = Snapshot {
            name: name.into(),
            created: now(),
            uid_base: b.uid_base,
            image: b.image.clone(),
            layers: b.layers.clone(),
            project: b.project.clone(),
            branch: b.branch.clone(),
            services: b.services.clone(),
        };
        let made = self.frozen(&b, || {
            for part in PARTS {
                copy_tree(&b.dir().join(part), &tmp.join(part))?;
            }
            Ok(())
        });
        if let Err(e) = made.and_then(|()| {
            fs::write(tmp.join("snapshot.json"), serde_json::to_vec_pretty(&snap)?)?;
            Ok(fs::rename(&tmp, &target)?)
        }) {
            let _ = fs::remove_dir_all(&tmp);
            return Err(e);
        }
        Ok(snap)
    }

    pub fn snapshots(&self, id: &str) -> anyhow::Result<Vec<Snapshot>> {
        let b = self.get(id)?;
        let mut all: Vec<Snapshot> = super::store::list(&dir_of(&b), |p| p.join("snapshot.json").is_file())
            .into_iter()
            .filter_map(|n| fs::read(dir_of(&b).join(n).join("snapshot.json")).ok())
            .filter_map(|data| serde_json::from_slice(&data).ok())
            .collect();
        all.sort_by_key(|s| s.created);
        Ok(all)
    }

    fn snapshot_dir(&self, b: &Berth, name: &str) -> anyhow::Result<std::path::PathBuf> {
        let p = dir_of(b).join(name);
        if !valid_name(name) || !p.join("snapshot.json").is_file() {
            bail!("berth {} has no snapshot {name}", b.id);
        }
        Ok(p)
    }

    pub fn remove_snapshot(&self, id: &str, name: &str) -> anyhow::Result<()> {
        let lock = self.lock_of(id);
        let _held = lock.lock().unwrap();
        let b = self.get(id)?;
        let p = self.snapshot_dir(&b, name)?;
        fs::remove_dir_all(p)?;
        Ok(())
    }

    /// Puts the berth back as it was in the snapshot name: its files, its
    /// services. What ran is stopped; the berth starts again from there.
    pub fn restore(&self, id: &str, name: &str) -> anyhow::Result<Berth> {
        {
            let lock = self.lock_of(id);
            let _held = lock.lock().unwrap();
            let mut b = self.get(id)?;
            let snap_dir = self.snapshot_dir(&b, name)?;
            let snap: Snapshot = serde_json::from_slice(&fs::read(snap_dir.join("snapshot.json"))?)?;
            if snap.image != b.image || snap.layers != b.layers {
                bail!("snapshot {name} was taken of a berth made from other layers");
            }
            self.teardown(&b)?;
            b.state = State::Stopped;
            b.pid = 0;
            self.put(&b)?;
            let d = b.dir();
            for part in PARTS {
                let old = d.join(format!(".{part}.old"));
                let _ = fs::remove_dir_all(&old);
                // The copy goes in first, under a hidden name, so a failure
                // leaves the berth as it was.
                let new = d.join(format!(".{part}.restoring"));
                let _ = fs::remove_dir_all(&new);
                copy_tree(&snap_dir.join(part), &new)?;
                layer::rebase_owners(&new, snap.uid_base, b.uid_base)?;
                if d.join(part).exists() {
                    fs::rename(d.join(part), &old)?;
                }
                fs::rename(&new, d.join(part))?;
                let _ = fs::remove_dir_all(&old);
            }
            // The overlay's scratch directory must start empty.
            let _ = fs::remove_dir_all(d.join("work"));
            fs::create_dir(d.join("work"))?;
            b.services = snap.services;
            self.put(&b)?;
        }
        self.wake(id)
    }

    /// A new berth with the files and services of a berth, or of one of its
    /// snapshots: frozen for a moment while it's copied, if it runs. With
    /// branch, the fork's workspace moves to that branch.
    pub fn fork(&self, id: &str, snapshot: Option<&str>, branch: Option<&str>) -> anyhow::Result<Berth> {
        if let Some(br) = branch
            && (br.is_empty() || br.starts_with('-') || br.contains(['\0', '\n', ' ']))
        {
            bail!("bad branch name {br}");
        }
        let source = self.get(id)?;
        if branch.is_some() && source.project.is_none() {
            bail!("berth {id} has no project, so its fork has no branch to move to");
        }
        // A fork serves none of its source's host ports: those are taken.
        let mut fork = self.reserve(Berth { published: Vec::new(), ..source.clone() })?;
        if let Some(br) = branch {
            fork.branch = Some(br.to_string());
        }
        let lock = self.lock_of(&fork.id);
        let _held = lock.lock().unwrap();
        let made = (|| -> anyhow::Result<()> {
            fs::create_dir_all(fork.dir())?;
            let from_base = {
                let src_lock = self.lock_of(id);
                let _src_held = src_lock.lock().unwrap();
                let source = self.get(id)?;
                match snapshot {
                    Some(name) => {
                        let snap_dir = self.snapshot_dir(&source, name)?;
                        let snap: Snapshot = serde_json::from_slice(&fs::read(snap_dir.join("snapshot.json"))?)?;
                        fork.services = snap.services;
                        for part in PARTS {
                            copy_tree(&snap_dir.join(part), &fork.dir().join(part))?;
                        }
                        snap.uid_base
                    }
                    None => {
                        self.frozen(&source, || {
                            for part in PARTS {
                                copy_tree(&source.dir().join(part), &fork.dir().join(part))?;
                            }
                            Ok(())
                        })?;
                        source.uid_base
                    }
                }
            };
            for part in PARTS {
                layer::rebase_owners(&fork.dir().join(part), from_base, fork.uid_base)?;
            }
            for sub in ["work", "root"] {
                fs::create_dir_all(fork.dir().join(sub))?;
            }
            chown(fork.dir(), Some(fork.uid_base), Some(fork.uid_base))?;
            fs::set_permissions(fork.dir(), fs::Permissions::from_mode(0o700))?;
            fork.save()?;
            self.boot(&mut fork)?;
            if let Some(br) = branch {
                let spec = crate::api::ExecSpec {
                    argv: ["git", "-C", "/workspace", "checkout", "-q", "-B", br].map(String::from).to_vec(),
                    ..Default::default()
                };
                let (code, out) = super::spawn::exec_collect(&fork, &spec)?;
                if code != 0 {
                    bail!("moving the fork to {br}: {}", out.trim());
                }
            }
            Ok(())
        })();
        match made {
            Ok(()) => {
                self.put(&fork)?;
                self.touch(&fork.id);
                Ok(fork)
            }
            Err(e) => {
                self.remove(&fork);
                Err(e)
            }
        }
    }
}
