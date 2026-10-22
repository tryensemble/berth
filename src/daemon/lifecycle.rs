//! Making, stopping and removing berths.

use std::fs;
use std::io::{BufRead, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{PermissionsExt, chown};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};

use super::berth::{Berth, CreateSpec, Limits, now, parse_size, random_id};
use super::spawn::{new_userns, spawn_init};
use super::{AGENT_UID, Daemon, UID_COUNT, UID_START, paths, repo};
use crate::init::InitConfig;
use crate::{image, net, sys};

impl Daemon {
    pub fn create(&self, spec: CreateSpec) -> anyhow::Result<Berth> {
        let image_root = image::rootfs(&paths::state("images"), &spec.image);
        if !image_root.exists() {
            image::pull(&paths::state("images"), &spec.image)?;
        }
        let mut lowers = vec![image_root];
        for l in &spec.layers {
            let p = paths::state("layers").join(l);
            if !p.exists() {
                bail!("no layer {l}; build it with `berth layer`");
            }
            lowers.push(p);
        }
        let mirror = match &spec.project {
            Some(p) => {
                let m = paths::state("repos").join(format!("{p}.git"));
                if !m.exists() {
                    bail!("no project {p}; add it with `berth repo`");
                }
                Some(m)
            }
            None => None,
        };

        // Reserve a number (hence ids and an address) under the lock.
        let b = {
            let mut berths = self.berths.lock().unwrap();
            let used: Vec<u32> = berths.values().map(|b| b.n).collect();
            let n = (2..).find(|n| !used.contains(n)).unwrap();
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
            };
            berths.insert(b.id.clone(), b.clone());
            b
        };
        match self.build(b.clone(), &lowers, mirror) {
            Ok(b) => {
                self.berths.lock().unwrap().insert(b.id.clone(), b.clone());
                Ok(b)
            }
            Err(e) => {
                let _ = self.destroy(&b.id);
                Err(e)
            }
        }
    }

    fn build(&self, mut b: Berth, lowers: &[PathBuf], mirror: Option<PathBuf>) -> anyhow::Result<Berth> {
        let d = b.dir();
        let base = b.uid_base;
        for sub in ["upper", "work", "root", "workspace"] {
            fs::create_dir_all(d.join(sub))?;
        }
        fs::set_permissions(&d, fs::Permissions::from_mode(0o711))?;
        chown(d.join("upper"), Some(base), Some(base))?;
        chown(d.join("workspace"), Some(base + AGENT_UID), Some(base + AGENT_UID))?;
        b.save()?;

        // The berth's filesystem: the image and layers, shifted into its id
        // range through idmapped mounts, under an overlay with its own upper.
        let userns = new_userns(base)?;
        let mut targets = Vec::new();
        for (i, lower) in lowers.iter().enumerate() {
            let t = d.join(format!("lower{i}"));
            fs::create_dir_all(&t)?;
            sys::idmapped_bind(lower, &t, userns.as_raw_fd())
                .with_context(|| format!("idmapped mount of {}", lower.display()))?;
            targets.push(t);
        }
        let lowerdir: Vec<String> = targets.iter().rev().map(|t| t.display().to_string()).collect();
        let data = format!(
            "lowerdir={},upperdir={},workdir={}",
            lowerdir.join(":"),
            d.join("upper").display(),
            d.join("work").display()
        );
        let mounted = sys::mount(Some("overlay"), &d.join("root"), Some("overlay"), 0, Some(&data));
        // The overlay keeps its own references to its layers.
        for t in &targets {
            let _ = sys::umount(t, libc::MNT_DETACH);
            let _ = fs::remove_dir(t);
        }
        mounted.context("mounting the berth's root")?;

        let cg = b.cgroup();
        fs::create_dir_all(&cg)?;
        let mem = parse_size(&b.limits.memory)?;
        fs::write(cg.join("cpu.max"), format!("{} 100000", (b.limits.cpus * 100_000.0) as u64))?;
        fs::write(cg.join("memory.max"), mem.to_string())?;
        fs::write(cg.join("memory.high"), (mem / 4 * 3).to_string())?;
        fs::write(cg.join("pids.max"), b.limits.pids.to_string())?;

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
            mirror: mirror.clone(),
            dns: net::resolvers(),
            tmp_size: b.limits.tmp.clone(),
        };
        let (pid, mut go, mut out) = spawn_init(&b, &cfg, &userns, &listener)?;
        drop(listener);
        b.pid = pid;
        b.save()?;

        net::attach(b.n, pid).context("attaching the berth's network")?;
        go.write_all(b"go\n")?;
        let mut line = String::new();
        out.read_line(&mut line)?;
        if line.trim() != "ready" {
            let log = fs::read_to_string(d.join("init.log")).unwrap_or_default();
            bail!("the berth didn't start: {}", log.trim());
        }

        if let (Some(mirror), Some(project)) = (&mirror, &b.project) {
            repo::check_out(&b, project, mirror)?;
        }
        Ok(b)
    }

    pub fn stop(&self, b: &Berth) {
        let cg = b.cgroup();
        if cg.exists() {
            let _ = fs::write(cg.join("cgroup.kill"), "1");
            for _ in 0..50 {
                let events = fs::read_to_string(cg.join("cgroup.events")).unwrap_or_default();
                if events.contains("populated 0") {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        net::detach(b.n);
        let _ = sys::umount(&b.dir().join("root"), libc::MNT_DETACH);
    }

    pub fn destroy(&self, id: &str) -> anyhow::Result<()> {
        let b = self.get(id)?;
        self.stop(&b);
        let _ = fs::remove_dir_all(b.run_dir());
        let _ = fs::remove_dir(b.cgroup());
        if b.dir().join("root").read_dir().map(|mut r| r.next().is_some()).unwrap_or(false) {
            bail!("berth {id}'s root is still mounted; not removing its files");
        }
        fs::remove_dir_all(b.dir())
            .or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) })?;
        self.berths.lock().unwrap().remove(id);
        Ok(())
    }
}
