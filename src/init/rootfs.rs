//! The berth's filesystem: its overlay root with /proc, /sys, its cgroup, a
//! minimal /dev, /tmp, the workspace and the daemon's other mounts (the
//! project mirror, Docker's data), pivoted into.
//!
//! The root holds what the berth wrote in earlier runs, so every path inside
//! it is resolved without following its links: nothing done here as the
//! berth's root may land outside the berth.

use std::fs::{self, File};
use std::os::unix::fs::symlink;
use std::path::Path;

use anyhow::Context;
use libc::{MS_BIND, MS_NODEV, MS_NOEXEC, MS_NOSUID, MS_PRIVATE, MS_RDONLY, MS_REC, MS_REMOUNT};

use super::InitConfig;
use crate::sys::{self, beneath};

/// Files in /proc that describe the host rather than the berth, or reach into
/// its kernel: hidden behind /dev/null.
const MASKED_FILES: &[&str] = &["kcore", "keys", "timer_list", "sched_debug", "sysrq-trigger", "kallsyms"];
/// Directories in /proc and /sys that do the same: hidden behind empty tmpfs.
const MASKED_DIRS: &[&str] = &["proc/acpi", "proc/scsi", "sys/firmware", "sys/kernel/debug", "sys/kernel/security"];

/// Builds the berth's filesystem under cfg.root and makes it the root.
pub fn build_root(cfg: &InitConfig) -> anyhow::Result<()> {
    let root = &cfg.root;
    sys::mount(None, Path::new("/"), None, MS_REC | MS_PRIVATE, None)?;
    sys::mount(Some(path_str(root)), root, None, MS_BIND | MS_REC, None)?;
    // Mount points belong to berth: whatever the berth left there goes.
    let mount_point = |p: &str| beneath::ensure_dir(root, Path::new(p), true);

    let proc = mount_point("proc")?;
    sys::mount(Some("proc"), &proc, Some("proc"), MS_NOSUID | MS_NODEV | MS_NOEXEC, None)?;
    let sys_dir = mount_point("sys")?;
    match sys::mount(Some("sysfs"), &sys_dir, Some("sysfs"), MS_RDONLY | MS_NOSUID | MS_NODEV | MS_NOEXEC, None) {
        Ok(()) => {
            // The berth's own cgroup, delegated to it: Docker builds on it.
            let cg = root.join("sys/fs/cgroup");
            if cg.is_dir() {
                sys::mount(Some("cgroup2"), &cg, Some("cgroup2"), MS_NOSUID | MS_NODEV | MS_NOEXEC, None)
                    .context("mounting the berth's cgroup")?;
            }
        }
        Err(e) => eprintln!("berth-init: no /sys: {e}"),
    }
    mask(root)?;

    build_dev(&mount_point("dev")?)?;
    let tmp = mount_point("tmp")?;
    let size = format!("mode=1777,size={}", cfg.tmp_size);
    sys::mount(Some("tmpfs"), &tmp, Some("tmpfs"), MS_NOSUID | MS_NODEV, Some(&size))?;

    let workspace = mount_point("workspace")?;
    bind(&cfg.workspace, &workspace, false)?;
    // The project mirror, if any, is already mounted at /berth/repo.git by
    // the daemon; the recursive bind of the root above carries it in.
    for b in &cfg.binds {
        bind(&b.source, &mount_point(&b.target)?, b.read_only).with_context(|| format!("mounting {}", b.target))?;
    }
    mount_point("var/log/berth")?;

    let resolv: String = cfg.dns.iter().map(|d| format!("nameserver {d}\n")).collect();
    let hosts = format!("127.0.0.1 localhost\n::1 localhost\n127.0.1.1 {}\n", cfg.hostname);
    for (file, data) in [("resolv.conf", resolv), ("hostname", format!("{}\n", cfg.hostname)), ("hosts", hosts)] {
        beneath::write(root, &Path::new("etc").join(file), data.as_bytes())
            .with_context(|| format!("writing /etc/{file}"))?;
    }

    let old = mount_point(".oldroot")?;
    let (r, o) = (sys::cstr(root), sys::cstr(&old));
    sys::cvt(unsafe { libc::syscall(libc::SYS_pivot_root, r.as_ptr(), o.as_ptr()) as libc::c_int })
        .context("pivot_root")?;
    std::env::set_current_dir("/")?;
    sys::umount(Path::new("/.oldroot"), libc::MNT_DETACH)?;
    fs::remove_dir("/.oldroot")?;
    Ok(())
}

/// Binds a host directory in. Its devices stay shut, as on the root.
fn bind(source: &Path, target: &Path, read_only: bool) -> anyhow::Result<()> {
    sys::mount(Some(path_str(source)), target, None, MS_BIND, None)?;
    let ro = if read_only { MS_RDONLY } else { 0 };
    // A remount in a user namespace must keep the flags the host's mount
    // locked (nosuid, noexec, atime), so it starts from them.
    let flags = held_flags(target)? | MS_BIND | MS_REMOUNT | MS_NODEV | ro;
    sys::mount(None, target, None, flags, None)?;
    Ok(())
}

/// The mount flags path's mount has now, as mount(2) flags.
fn held_flags(path: &Path) -> anyhow::Result<libc::c_ulong> {
    let p = sys::cstr(path);
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    sys::cvt(unsafe { libc::statvfs(p.as_ptr(), &mut st) })?;
    let f = st.f_flag;
    // Not in the libc crate's musl bindings.
    const ST_RELATIME: libc::c_ulong = 0x1000;
    let map = [
        (libc::ST_RDONLY, MS_RDONLY),
        (libc::ST_NOSUID, MS_NOSUID),
        (libc::ST_NODEV, MS_NODEV),
        (libc::ST_NOEXEC, MS_NOEXEC),
        (libc::ST_NOATIME, libc::MS_NOATIME),
        (libc::ST_NODIRATIME, libc::MS_NODIRATIME),
        (ST_RELATIME, libc::MS_RELATIME),
    ];
    Ok(map.iter().filter(|(st, _)| f & st != 0).fold(0, |acc, (_, ms)| acc | ms))
}

/// Hides what in /proc and /sys tells about, or reaches into, the host.
fn mask(root: &Path) -> anyhow::Result<()> {
    for f in MASKED_FILES {
        let p = root.join("proc").join(f);
        if p.exists() {
            sys::mount(Some("/dev/null"), &p, None, MS_BIND, None)?;
        }
    }
    for d in MASKED_DIRS {
        let p = root.join(d);
        if p.is_dir() {
            sys::mount(
                Some("tmpfs"),
                &p,
                Some("tmpfs"),
                MS_RDONLY | MS_NOSUID | MS_NODEV | MS_NOEXEC,
                Some("size=4k"),
            )?;
        }
    }
    // Kernel tunables stay readable, not writable.
    let sysctl = root.join("proc/sys");
    sys::mount(Some(path_str(&sysctl)), &sysctl, None, MS_BIND | MS_REC, None)?;
    sys::mount(None, &sysctl, None, MS_BIND | MS_REMOUNT | MS_RDONLY | MS_NOSUID | MS_NODEV | MS_NOEXEC, None)?;
    // Except the berth's own network's, which Docker and others set.
    let net = root.join("proc/sys/net");
    sys::mount(Some(path_str(&net)), &net, None, MS_BIND | MS_REC, None)?;
    sys::mount(None, &net, None, MS_BIND | MS_REMOUNT | MS_NOSUID | MS_NODEV | MS_NOEXEC, None)?;
    Ok(())
}

/// A minimal /dev on a fresh tmpfs: the harmless devices, a private devpts.
fn build_dev(dev: &Path) -> anyhow::Result<()> {
    sys::mount(Some("tmpfs"), dev, Some("tmpfs"), MS_NOSUID | MS_NOEXEC, Some("mode=755,size=64k"))?;
    for name in ["null", "zero", "full", "random", "urandom", "tty"] {
        let target = dev.join(name);
        File::create(&target)?;
        sys::mount(Some(&format!("/dev/{name}")), &target, None, MS_BIND, None)?;
    }
    if Path::new("/dev/fuse").exists() {
        let target = dev.join("fuse");
        File::create(&target)?;
        sys::mount(Some("/dev/fuse"), &target, None, MS_BIND, None)?;
    }
    fs::create_dir(dev.join("pts"))?;
    sys::mount(
        Some("devpts"),
        &dev.join("pts"),
        Some("devpts"),
        MS_NOSUID | MS_NOEXEC,
        Some("newinstance,ptmxmode=0666,mode=0620,gid=5"),
    )?;
    symlink("pts/ptmx", dev.join("ptmx"))?;
    fs::create_dir(dev.join("shm"))?;
    sys::mount(Some("tmpfs"), &dev.join("shm"), Some("tmpfs"), MS_NOSUID | MS_NODEV, Some("mode=1777"))?;
    fs::create_dir(dev.join("mqueue"))?;
    let _ = sys::mount(Some("mqueue"), &dev.join("mqueue"), Some("mqueue"), MS_NOSUID | MS_NODEV | MS_NOEXEC, None);
    symlink("/proc/self/fd", dev.join("fd"))?;
    for (i, name) in ["stdin", "stdout", "stderr"].iter().enumerate() {
        symlink(format!("/proc/self/fd/{i}"), dev.join(name))?;
    }
    Ok(())
}

fn path_str(p: &Path) -> &str {
    p.to_str().expect("utf-8 path")
}
