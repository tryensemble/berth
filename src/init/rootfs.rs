//! The berth's filesystem: its overlay root with /proc, /sys, a minimal /dev,
//! /tmp, the workspace and the project's mirror, pivoted into.

use std::fs::{self, File};
use std::os::unix::fs::symlink;
use std::path::Path;

use anyhow::Context;

use super::InitConfig;
use crate::sys;

/// Builds the berth's filesystem under cfg.root and makes it the root.
pub fn build_root(cfg: &InitConfig) -> anyhow::Result<()> {
    use libc::{MS_BIND, MS_NODEV, MS_NOEXEC, MS_NOSUID, MS_PRIVATE, MS_RDONLY, MS_REC, MS_REMOUNT};
    let root = &cfg.root;
    sys::mount(None, Path::new("/"), None, MS_REC | MS_PRIVATE, None)?;
    sys::mount(Some(path_str(root)), root, None, MS_BIND | MS_REC, None)?;
    let at = |p: &str| root.join(p.trim_start_matches('/'));
    for d in ["proc", "sys", "dev", "tmp", "workspace", "berth", ".oldroot", "var/log/berth"] {
        fs::create_dir_all(at(d))?;
    }

    sys::mount(Some("proc"), &at("proc"), Some("proc"), MS_NOSUID | MS_NODEV | MS_NOEXEC, None)?;
    if let Err(e) =
        sys::mount(Some("sysfs"), &at("sys"), Some("sysfs"), MS_RDONLY | MS_NOSUID | MS_NODEV | MS_NOEXEC, None)
    {
        eprintln!("berth-init: no /sys: {e}");
    }

    let dev = at("dev");
    sys::mount(Some("tmpfs"), &dev, Some("tmpfs"), MS_NOSUID | MS_NOEXEC, Some("mode=755,size=64k"))?;
    for name in ["null", "zero", "full", "random", "urandom", "tty"] {
        let target = dev.join(name);
        File::create(&target)?;
        sys::mount(Some(&format!("/dev/{name}")), &target, None, MS_BIND, None)?;
    }
    fs::create_dir_all(dev.join("pts"))?;
    sys::mount(
        Some("devpts"),
        &dev.join("pts"),
        Some("devpts"),
        MS_NOSUID | MS_NOEXEC,
        Some("newinstance,ptmxmode=0666,mode=0620,gid=5"),
    )?;
    symlink("pts/ptmx", dev.join("ptmx"))?;
    fs::create_dir_all(dev.join("shm"))?;
    sys::mount(Some("tmpfs"), &dev.join("shm"), Some("tmpfs"), MS_NOSUID | MS_NODEV, Some("mode=1777"))?;
    symlink("/proc/self/fd", dev.join("fd"))?;
    for (i, name) in ["stdin", "stdout", "stderr"].iter().enumerate() {
        symlink(format!("/proc/self/fd/{i}"), dev.join(name))?;
    }

    sys::mount(
        Some("tmpfs"),
        &at("tmp"),
        Some("tmpfs"),
        MS_NOSUID | MS_NODEV,
        Some(&format!("mode=1777,size={}", cfg.tmp_size)),
    )?;
    sys::mount(Some(path_str(&cfg.workspace)), &at("workspace"), None, MS_BIND, None)?;
    if let Some(mirror) = &cfg.mirror {
        let target = at("berth/repo.git");
        fs::create_dir_all(&target)?;
        sys::mount(Some(path_str(mirror)), &target, None, MS_BIND, None)?;
        sys::mount(None, &target, None, MS_BIND | MS_REMOUNT | MS_RDONLY, None)?;
    }

    let etc = at("etc");
    for f in ["resolv.conf", "hostname", "hosts"] {
        let _ = fs::remove_file(etc.join(f));
    }
    let resolv: String = cfg.dns.iter().map(|d| format!("nameserver {d}\n")).collect();
    fs::write(etc.join("resolv.conf"), resolv)?;
    fs::write(etc.join("hostname"), format!("{}\n", cfg.hostname))?;
    fs::write(etc.join("hosts"), format!("127.0.0.1 localhost\n::1 localhost\n127.0.1.1 {}\n", cfg.hostname))?;

    let old = at(".oldroot");
    let (r, o) = (sys::cstr(root), sys::cstr(&old));
    sys::cvt(unsafe { libc::syscall(libc::SYS_pivot_root, r.as_ptr(), o.as_ptr()) as libc::c_int })
        .context("pivot_root")?;
    std::env::set_current_dir("/")?;
    sys::umount(Path::new("/.oldroot"), libc::MNT_DETACH)?;
    fs::remove_dir("/.oldroot")?;
    Ok(())
}

fn path_str(p: &Path) -> &str {
    p.to_str().expect("utf-8 path")
}
