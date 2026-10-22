//! Making, starting, idling, stopping and removing berths.
//!
//! Every change to one berth holds that berth's lock, so a wake can't race a
//! stop and two execs can't both start a stopped berth.

use std::fs;
use std::io::{BufRead, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{PermissionsExt, chown};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;

use anyhow::{Context, bail};

use super::berth::{Berth, CreateSpec, Limits, State, now, parse_duration, random_id};
use super::spawn::{new_userns, spawn_init};
use super::{AGENT_UID, Daemon, UID_COUNT, UID_START, cgroup, paths, repo, spawn, valid_name};
use crate::api::{ExecSpec, InitRequest};
use crate::init::{Bind, InitConfig};
use crate::{image, net, sys};

impl Daemon {
    pub fn create(&self, spec: CreateSpec) -> anyhow::Result<Berth> {
        let image_root = image::rootfs(&paths::state("images"), &spec.image);
        if !image_root.exists() {
            image::pull(&paths::state("images"), &spec.image)?;
        }
        for l in &spec.layers {
            if !valid_name(l) || !paths::state("layers").join(l).is_dir() {
                bail!("no layer {l}; build it with `berth layer`");
            }
        }
        if let Some(p) = &spec.project
            && (!valid_name(p) || !paths::state("repos").join(format!("{p}.git")).is_dir())
        {
            bail!("no project {p}; add it with `berth repo`");
        }
        if let Some(t) = &spec.tenant
            && !valid_name(t)
        {
            bail!("bad tenant name {t}");
        }
        let idle_after = spec.idle_after.as_deref().map(parse_duration).transpose()?;
        let stop_after = spec.stop_after.as_deref().map(parse_duration).transpose()?;

        // Reserve a number (hence ids and an address) under the lock.
        let mut b = {
            let mut berths = self.berths.lock().unwrap();
            let used: Vec<u32> = berths.values().map(|b| b.n).collect();
            let n = (2..net::MAX_BERTHS).find(|n| !used.contains(n)).context("this machine is full of berths")?;
            let b = Berth {
                id: random_id(),
                n,
                uid_base: UID_START + n * UID_COUNT,
                ip: net::address(n).to_string(),
                pid: 0,
                project: spec.project.clone(),
                branch: spec.branch.clone(),
                image: spec.image.clone(),
                layers: spec.layers.clone(),
                limits: Limits { cpus: spec.cpus, memory: spec.memory.clone(), pids: spec.pids, tmp: spec.tmp.clone() },
                created: now(),
                state: State::Stopped,
                idle_after: idle_after.unwrap_or(super::berth::DEFAULT_IDLE_AFTER),
                stop_after: stop_after.unwrap_or(super::berth::DEFAULT_STOP_AFTER),
                services: Vec::new(),
                tenant: spec.tenant.clone(),
            };
            berths.insert(b.id.clone(), b.clone());
            b
        };
        let lock = self.lock_of(&b.id);
        let _held = lock.lock().unwrap();
        let made = (|| -> anyhow::Result<()> {
            self.prepare(&b)?;
            self.boot(&mut b)?;
            if let (Some(project), Some(mirror)) = (&b.project, self.mirror_of(&b)) {
                repo::check_out(&b, project, &mirror)?;
            }
            Ok(())
        })();
        match made {
            Ok(()) => {
                self.put(&b)?;
                self.touch(&b.id);
                Ok(b)
            }
            Err(e) => {
                self.remove(&b);
                Err(e)
            }
        }
    }

    /// The berth's directories, made once.
    fn prepare(&self, b: &Berth) -> anyhow::Result<()> {
        let d = b.dir();
        for sub in ["upper", "work", "root", "workspace", "docker"] {
            fs::create_dir_all(d.join(sub))?;
        }
        // Init enters this directory as the berth's root; no one else may.
        let base = b.uid_base;
        chown(&d, Some(base), Some(base))?;
        fs::set_permissions(&d, fs::Permissions::from_mode(0o700))?;
        chown(d.join("upper"), Some(base), Some(base))?;
        chown(d.join("docker"), Some(base), Some(base))?;
        chown(d.join("workspace"), Some(base + AGENT_UID), Some(base + AGENT_UID))?;
        b.save()
    }

    fn mirror_of(&self, b: &Berth) -> Option<PathBuf> {
        b.project.as_ref().map(|p| paths::state("repos").join(format!("{p}.git")))
    }

    /// What's mounted in the berth besides its root, workspace and mirror:
    /// Docker's data on the host's disk, since overlay2 can't sit on the
    /// berth's own overlay.
    fn binds(&self, b: &Berth) -> Vec<Bind> {
        vec![Bind { source: b.dir().join("docker"), target: "var/lib/docker".into(), read_only: false }]
    }

    /// Starts a berth whose files are ready: its root, cgroup, init, network
    /// and services. Holds the berth's lock.
    fn boot(&self, b: &mut Berth) -> anyhow::Result<()> {
        let d = b.dir();
        // Berths from before Docker came have no directory for it.
        if !d.join("docker").exists() {
            fs::create_dir(d.join("docker"))?;
            chown(d.join("docker"), Some(b.uid_base), Some(b.uid_base))?;
        }
        self.mount_root(b)?;
        // The daemon, as root, mounts the mirror into the berth's root before
        // init unshares its mount namespace, so init never reads the
        // root-only repos directory. The mount is carried into the berth.
        if let Some(mirror) = self.mirror_of(b) {
            let repo = d.join("root/berth/repo.git");
            fs::create_dir_all(&repo)?;
            sys::mount(Some(&mirror.display().to_string()), &repo, None, libc::MS_BIND, None)?;
            sys::mount(None, &repo, None, libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY, None)?;
        }
        cgroup::create(b)?;

        let rd = b.run_dir();
        fs::create_dir_all(&rd)?;
        let sock = rd.join("init.sock");
        let _ = fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock)?;
        fs::set_permissions(&sock, fs::Permissions::from_mode(0o600))?;

        let cfg = InitConfig {
            id: b.id.clone(),
            hostname: format!("berth-{}", b.id),
            root: d.join("root"),
            workspace: d.join("workspace"),
            dns: net::resolvers(),
            tmp_size: b.limits.tmp.clone(),
            binds: self.binds(b),
            docker: Default::default(),
        };
        let userns = new_userns(b.uid_base)?;
        let (pid, mut go, mut out) = spawn_init(b, &cfg, &userns, &listener)?;
        drop(listener);
        b.pid = pid;
        b.save()?;

        cgroup::settle(b, pid)?;
        net::attach(b.n, pid).context("attaching the berth's network")?;
        go.write_all(b"go\n")?;
        let mut line = String::new();
        out.read_line(&mut line)?;
        if line.trim() != "ready" {
            let log = fs::read_to_string(d.join("init.log")).unwrap_or_default();
            bail!("the berth didn't start: {}", log.trim());
        }
        b.state = State::Running;
        for service in b.services.clone() {
            if let Err(e) = spawn::start_detached(b, &service) {
                eprintln!("berthd: berth {}: service {}: {e:#}", b.id, service.service.as_deref().unwrap_or(""));
            }
        }
        Ok(())
    }

    /// The berth's root: the image and layers, shifted into its id range
    /// through idmapped mounts, under an overlay with its own upper.
    fn mount_root(&self, b: &Berth) -> anyhow::Result<()> {
        let d = b.dir();
        let mut lowers = vec![image::rootfs(&paths::state("images"), &b.image)];
        lowers.extend(b.layers.iter().map(|l| paths::state("layers").join(l)));
        let userns = new_userns(b.uid_base)?;
        let mut targets = Vec::new();
        let mut mounted = Ok(());
        for (i, lower) in lowers.iter().enumerate() {
            let t = d.join(format!("lower{i}"));
            fs::create_dir_all(&t)?;
            mounted = sys::idmapped_bind(lower, &t, userns.as_raw_fd())
                .with_context(|| format!("idmapped mount of {}", lower.display()));
            targets.push(t);
            if mounted.is_err() {
                break;
            }
        }
        if mounted.is_ok() {
            let lowerdir: Vec<String> = targets.iter().rev().map(|t| t.display().to_string()).collect();
            let data = format!(
                "lowerdir={},upperdir={},workdir={}",
                lowerdir.join(":"),
                d.join("upper").display(),
                d.join("work").display()
            );
            mounted = sys::mount(Some("overlay"), &d.join("root"), Some("overlay"), 0, Some(&data))
                .context("mounting the berth's root");
        }
        // The overlay keeps its own references to its layers.
        for t in &targets {
            let _ = sys::umount(t, libc::MNT_DETACH);
            let _ = fs::remove_dir(t);
        }
        mounted
    }

    /// Ends everything the berth runs and takes down its mounts and network;
    /// its files stay.
    pub(super) fn teardown(&self, b: &Berth) -> anyhow::Result<()> {
        if !cgroup::kill(b) {
            bail!("berth {} still has running processes; leaving it in place", b.id);
        }
        cgroup::remove(&b.cgroup());
        net::detach(b.n);
        let _ = sys::umount(&b.dir().join("root/berth/repo.git"), libc::MNT_DETACH);
        let _ = sys::umount(&b.dir().join("root"), libc::MNT_DETACH);
        let _ = fs::remove_dir_all(b.run_dir());
        Ok(())
    }

    /// The berth, running: thawed if idle, started if stopped.
    pub fn wake(&self, id: &str) -> anyhow::Result<Berth> {
        let lock = self.lock_of(id);
        let _held = lock.lock().unwrap();
        let mut b = self.get(id)?;
        match b.state {
            State::Running if b.alive() => {}
            State::Idle if b.alive() => {
                cgroup::freeze(&b, false)?;
                b.state = State::Running;
                self.put(&b)?;
            }
            _ => {
                self.teardown(&b)?;
                if let Err(e) = self.boot(&mut b) {
                    let _ = self.teardown(&b);
                    b.state = State::Stopped;
                    self.put(&b)?;
                    return Err(e.context(format!("starting berth {id}")));
                }
                self.put(&b)?;
            }
        }
        self.touch(id);
        Ok(b)
    }

    /// Freezes a running berth and pushes its memory out; with
    /// if_unused_for, only if it's been unused that many seconds.
    pub fn idle(&self, id: &str, if_unused_for: Option<u64>) -> anyhow::Result<Berth> {
        let lock = self.lock_of(id);
        let _held = lock.lock().unwrap();
        let mut b = self.get(id)?;
        if if_unused_for.is_some_and(|secs| self.unused_for(id).as_secs() < secs) {
            return Ok(b);
        }
        if b.state == State::Running && b.alive() {
            cgroup::freeze(&b, true)?;
            cgroup::reclaim(&b);
            b.state = State::Idle;
            self.put(&b)?;
        }
        Ok(b)
    }

    /// Ends everything in a berth; its files and services stay for its next
    /// start.
    pub fn stop(&self, id: &str, if_unused_for: Option<u64>) -> anyhow::Result<Berth> {
        let lock = self.lock_of(id);
        let _held = lock.lock().unwrap();
        let mut b = self.get(id)?;
        if if_unused_for.is_some_and(|secs| self.unused_for(id).as_secs() < secs) {
            return Ok(b);
        }
        self.teardown(&b)?;
        b.state = State::Stopped;
        b.pid = 0;
        self.put(&b)?;
        Ok(b)
    }

    pub fn destroy(&self, id: &str) -> anyhow::Result<()> {
        let lock = self.lock_of(id);
        let _held = lock.lock().unwrap();
        let b = self.get(id)?;
        self.remove(&b);
        if b.dir().exists() {
            bail!("berth {id}'s files couldn't all be removed");
        }
        Ok(())
    }

    /// Takes a berth down and removes its files and record. Holds its lock.
    fn remove(&self, b: &Berth) {
        if let Err(e) = self.teardown(b) {
            eprintln!("berthd: {e:#}");
            return;
        }
        if b.dir().join("root").read_dir().map(|mut r| r.next().is_some()).unwrap_or(false) {
            eprintln!("berthd: berth {}'s root is still mounted; keeping its files", b.id);
            return;
        }
        if let Err(e) = fs::remove_dir_all(b.dir())
            && e.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("berthd: removing berth {}: {e}", b.id);
            return;
        }
        self.berths.lock().unwrap().remove(&b.id);
        self.forget(&b.id);
    }

    /// Records a service and starts it; it starts again whenever the berth
    /// does. One of the same name is replaced.
    pub fn add_service(&self, id: &str, spec: ExecSpec) -> anyhow::Result<u32> {
        let name = spec.service.clone().context("no service name")?;
        if !valid_name(&name) {
            bail!("bad service name {name}");
        }
        let b = self.wake(id)?;
        let lock = self.lock_of(id);
        let _held = lock.lock().unwrap();
        let mut b = self.get(&b.id)?;
        b.services.retain(|s| s.service.as_deref() != Some(&name));
        b.services.push(spec.clone());
        self.put(&b)?;
        spawn::start_detached(&b, &spec)
    }

    /// Stops a service and forgets it.
    pub fn remove_service(&self, id: &str, name: &str) -> anyhow::Result<()> {
        let lock = self.lock_of(id);
        let _held = lock.lock().unwrap();
        let mut b = self.get(id)?;
        let before = b.services.len();
        b.services.retain(|s| s.service.as_deref() != Some(name));
        if b.services.len() == before {
            bail!("berth {id} has no service {name}");
        }
        self.put(&b)?;
        if b.alive() {
            let was_idle = b.state == State::Idle;
            if was_idle {
                cgroup::freeze(&b, false)?;
            }
            spawn::ask_init(&b, &InitRequest::StopService(name.to_string()))?;
            if was_idle {
                cgroup::freeze(&b, true)?;
            }
        }
        Ok(())
    }

    /// Keeps what a berth installed (its upper layer) as a read-only layer
    /// other berths can be made from, and removes the berth.
    pub fn commit_layer(&self, id: &str, name: &str) -> anyhow::Result<()> {
        if !valid_name(name) {
            bail!("bad layer name {name}");
        }
        let lock = self.lock_of(id);
        let _held = lock.lock().unwrap();
        self.unused("layer", name, |b| b.layers.iter().any(|l| l == name) && b.id != id)?;
        let b = self.get(id)?;
        self.teardown(&b)?;
        super::layer::commit(&b, name)?;
        self.remove(&b);
        Ok(())
    }
}
