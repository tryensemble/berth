//! Unpacking image layers, with OCI whiteouts, in a process of their own.

use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::bail;

use crate::sys::{self, seccomp};

/// Unpacks the layer in blob into tree, an idmapped mount of the image's
/// root (see [`sys::idmapped_tree`]), in a process that can do nothing else.
/// It runs as the unprivileged ids userns maps to, which own nothing on the
/// host, with no network, under no_new_privs and the berth's own seccomp
/// filter, chrooted into the tree: a link or path in the layer has nowhere to
/// lead, and a bug in the tar or gzip code has nothing to reach.
pub fn isolated(tree: &OwnedFd, userns: &OwnedFd, blob: &Path, gzip: bool) -> anyhow::Result<()> {
    let (t, u) = (tree.as_raw_fd(), userns.as_raw_fd());
    let mut cmd = Command::new("/proc/self/exe");
    cmd.arg0("berth-unpack").args(["__unpack", "--tree-fd", &t.to_string()]);
    if gzip {
        cmd.arg("--gzip");
    }
    cmd.env_clear().stdin(File::open(blob)?).stdout(Stdio::null()).stderr(Stdio::piped());
    unsafe {
        let parent = libc::getpid();
        cmd.pre_exec(move || {
            sys::setns(u, libc::CLONE_NEWUSER)?;
            // Become the namespace's root (the first unpack id): host root
            // isn't mapped in it, and an unmapped uid execs without caps.
            sys::cvt(libc::setgroups(0, std::ptr::null()))?;
            sys::cvt(libc::setresgid(0, 0, 0))?;
            sys::cvt(libc::setresuid(0, 0, 0))?;
            // After the change of ids, which would clear it: die with the
            // daemon, and don't start if it's gone already.
            sys::cvt(libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL))?;
            if libc::getppid() != parent {
                return Err(io::Error::from_raw_os_error(libc::ESRCH));
            }
            // Nothing of the host's but the tree: no network, IPC, mounts,
            // hostname or cgroup view.
            let alone = libc::CLONE_NEWNET | libc::CLONE_NEWIPC | libc::CLONE_NEWNS | libc::CLONE_NEWUTS;
            sys::cvt(libc::unshare(alone | libc::CLONE_NEWCGROUP))?;
            // No fd the daemon holds reaches it, but the tree's.
            const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;
            let cloexec = |from: i32, to: libc::c_uint| {
                libc::syscall(libc::SYS_close_range, from as libc::c_uint, to, CLOSE_RANGE_CLOEXEC)
            };
            if t > 3 {
                cloexec(3, t as libc::c_uint - 1);
            }
            cloexec(t + 1, libc::c_uint::MAX);
            sys::cvt(libc::fcntl(t, libc::F_SETFD, 0))?;
            Ok(())
        });
    }
    let out = cmd.output()?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        bail!("{}", msg.trim().trim_start_matches("berth: "));
    }
    Ok(())
}

/// The unpacking process, from [`isolated`]: confines itself, then unpacks
/// the layer on stdin into the tree at tree_fd.
pub fn child(tree_fd: i32, gzip: bool) -> anyhow::Result<()> {
    let tree = unsafe { OwnedFd::from_raw_fd(tree_fd) };
    sys::cvt(unsafe { libc::fchdir(tree.as_raw_fd()) })?;
    sys::cvt(unsafe { libc::chroot(c".".as_ptr()) })?;
    std::env::set_current_dir("/")?;
    drop(tree);
    // One process, no core dumps.
    for (res, n) in [(libc::RLIMIT_NPROC, 1), (libc::RLIMIT_CORE, 0)] {
        sys::cvt(unsafe { libc::setrlimit(res, &libc::rlimit { rlim_cur: n, rlim_max: n }) })?;
    }
    sys::cvt(unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) })?;
    seccomp::install(&seccomp::program())?;
    unpack_layer(unsafe { File::from_raw_fd(0) }, Path::new("/"), gzip)
}

/// Unpacks a layer tarball over root. `.wh.<name>` removes name from the
/// layers below; `.wh..wh..opq` empties its directory first. Device nodes are
/// left out: a berth gets its devices from its own /dev, never from an image.
pub fn unpack_layer(file: File, root: &Path, gzip: bool) -> anyhow::Result<()> {
    let reader: Box<dyn Read> = if gzip { Box::new(flate2::read::GzDecoder::new(file)) } else { Box::new(file) };
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    // Owners are set below, kept within the ids a berth has.
    archive.set_preserve_ownerships(false);
    // Extended attributes are set below, only the ones an image may carry.
    archive.set_unpack_xattrs(false);
    archive.set_overwrite(true);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        if kind.is_block_special() || kind.is_character_special() {
            continue;
        }
        let path = entry.path()?.into_owned();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if let Some(hidden) = name.strip_prefix(".wh.") {
            // Whiteouts are removals: keep them from reaching outside root,
            // through `..` or a symlink an earlier layer left on the way.
            let Some(parent) = dir_inside(root, path.parent().unwrap_or(Path::new(""))) else { continue };
            if hidden == ".wh..opq" {
                for e in fs::read_dir(&parent)?.flatten() {
                    remove_any(&e.path())?;
                }
            } else if !matches!(hidden, "" | "." | "..") {
                remove_any(&parent.join(hidden))?;
            }
            continue;
        }
        // A link to something left out (a device node) is left out too.
        if kind.is_hard_link()
            && let Some(target) = entry.link_name()?
            && fs::symlink_metadata(root.join(&target)).is_err()
        {
            continue;
        }
        let h = entry.header();
        let (uid, gid, mode) = (owner_id(h.uid()?), owner_id(h.gid()?), h.mode()?);
        let xattrs = xattrs_of(&mut entry)?;
        if entry.unpack_in(root)? {
            let dest = root.join(&path);
            if !kind.is_hard_link() {
                own(&dest, uid, gid, (!kind.is_symlink()).then_some(mode))?;
            }
            set_xattrs(&dest, &xattrs);
        }
    }
    Ok(())
}

/// The id a file gets for an owner an image gave it: as given if a berth has
/// it (0..65536), else nobody's, as a kernel shows ids it can't map.
fn owner_id(id: u64) -> u32 {
    if id < 65536 { id as u32 } else { 65534 }
}

/// Gives path (itself, not what a link there points to) its owners, then its
/// mode again, since a change of owner clears setuid and setgid bits.
fn own(path: &Path, uid: u32, gid: u32, mode: Option<u32>) -> io::Result<()> {
    std::os::unix::fs::lchown(path, Some(uid), Some(gid))?;
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777))?;
    }
    Ok(())
}

/// The extended attributes an entry carries that an image may set: file
/// capabilities (ping's, say) and user.* ones. Not trusted.* or user.overlay.*,
/// which overlayfs reads its own state from, nor security labels.
fn xattrs_of<R: Read>(entry: &mut tar::Entry<R>) -> io::Result<Vec<(CString, Vec<u8>)>> {
    let mut v = Vec::new();
    let Some(exts) = entry.pax_extensions()? else { return Ok(v) };
    for ext in exts {
        let ext = ext?;
        let Some(name) = ext.key_bytes().strip_prefix(b"SCHILY.xattr.") else { continue };
        let wanted = name == b"security.capability" || name.starts_with(b"user.");
        // An overlay mounted in a user namespace reads its own state from these.
        if wanted
            && !name.starts_with(b"user.overlay.")
            && let Ok(name) = CString::new(name)
        {
            v.push((name, ext.value_bytes().to_vec()));
        }
    }
    Ok(v)
}

/// Sets xattrs on path itself, not on what a link there points to. One that
/// won't set (a filesystem without user xattrs) is left off, as Docker does.
fn set_xattrs(path: &Path, xattrs: &[(CString, Vec<u8>)]) {
    let Ok(p) = CString::new(path.as_os_str().as_bytes()) else { return };
    for (name, value) in xattrs {
        unsafe { libc::lsetxattr(p.as_ptr(), name.as_ptr(), value.as_ptr() as *const _, value.len(), 0) };
    }
}

/// root/rel with its symlinks resolved, if that's a directory inside root.
fn dir_inside(root: &Path, rel: &Path) -> Option<PathBuf> {
    if rel.components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir)) {
        return None;
    }
    let dir = fs::canonicalize(root.join(rel)).ok()?;
    (dir.starts_with(fs::canonicalize(root).ok()?) && dir.is_dir()).then_some(dir)
}

fn remove_any(p: &Path) -> io::Result<()> {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(p),
        Ok(_) => fs::remove_file(p),
        Err(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, Write};

    enum E<'a> {
        Dir(&'a str),
        File(&'a str, &'a [u8]),
    }

    /// A header owned by whoever runs the tests, since unpacking keeps owners.
    fn header() -> tar::Header {
        let mut h = tar::Header::new_gnu();
        h.set_uid(unsafe { libc::getuid() } as u64);
        h.set_gid(unsafe { libc::getgid() } as u64);
        h
    }

    fn layer(entries: &[E]) -> File {
        let mut b = tar::Builder::new(tempfile::tempfile().unwrap());
        for e in entries {
            let mut h = header();
            match e {
                E::Dir(path) => {
                    h.set_entry_type(tar::EntryType::Directory);
                    h.set_mode(0o755);
                    h.set_size(0);
                    b.append_data(&mut h, path, io::empty()).unwrap();
                }
                E::File(path, data) => {
                    h.set_mode(0o644);
                    h.set_size(data.len() as u64);
                    b.append_data(&mut h, path, *data).unwrap();
                }
            }
        }
        let mut f = b.into_inner().unwrap();
        f.rewind().unwrap();
        f
    }

    #[test]
    fn layers_stack_and_whiteouts_remove() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        let lower = layer(&[
            E::Dir("etc"),
            E::File("etc/keep", b"1"),
            E::File("etc/drop", b"2"),
            E::Dir("opaque"),
            E::File("opaque/old", b"3"),
        ]);
        let upper = layer(&[
            E::File("etc/.wh.drop", b""),
            E::File("etc/keep", b"new"),
            E::File("opaque/.wh..wh..opq", b""),
            E::File("opaque/fresh", b"4"),
        ]);
        unpack_layer(lower, r, false).unwrap();
        unpack_layer(upper, r, false).unwrap();

        assert_eq!(fs::read_to_string(r.join("etc/keep")).unwrap(), "new");
        assert!(!r.join("etc/drop").exists());
        assert!(!r.join("etc/.wh.drop").exists());
        assert!(!r.join("opaque/old").exists());
        assert!(!r.join("opaque/.wh..wh..opq").exists());
        assert!(r.join("opaque/fresh").exists());
    }

    #[test]
    fn gzipped_layers_unpack() {
        let mut plain = layer(&[E::File("hello", b"world")]);
        let mut gz = flate2::write::GzEncoder::new(tempfile::tempfile().unwrap(), flate2::Compression::fast());
        io::copy(&mut plain, &mut gz).unwrap();
        let mut f = gz.finish().unwrap();
        f.rewind().unwrap();

        let root = tempfile::tempdir().unwrap();
        unpack_layer(f, root.path(), true).unwrap();
        assert_eq!(fs::read_to_string(root.path().join("hello")).unwrap(), "world");
    }

    /// A raw tar entry, bypassing the builder's own path checks.
    fn raw_entry(name: &[u8]) -> File {
        let mut h = header();
        h.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name);
        h.set_mode(0o644);
        h.set_size(1);
        h.set_cksum();
        let mut b = tar::Builder::new(Vec::new());
        b.append(&h, &b"x"[..]).unwrap();
        let mut f = tempfile::tempfile().unwrap();
        f.write_all(&b.into_inner().unwrap()).unwrap();
        f.rewind().unwrap();
        f
    }

    #[test]
    fn entries_and_whiteouts_stay_inside_the_root() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::write(outer.path().join("victim"), "keep me").unwrap();

        let _ = unpack_layer(raw_entry(b"../escaped"), &root, false);
        assert!(!outer.path().join("escaped").exists());

        unpack_layer(raw_entry(b"../.wh.victim"), &root, false).unwrap();
        unpack_layer(raw_entry(b"sub/.wh.."), &root, false).unwrap();
        assert!(outer.path().join("victim").exists());
        assert!(root.exists());
    }

    #[test]
    fn whiteouts_dont_follow_symlinks_out() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::create_dir(outer.path().join("host")).unwrap();
        fs::write(outer.path().join("host/passwd"), "keep me").unwrap();
        // An earlier layer left a link out of the image.
        std::os::unix::fs::symlink(outer.path().join("host"), root.join("x")).unwrap();

        unpack_layer(raw_entry(b"x/.wh.passwd"), &root, false).unwrap();
        unpack_layer(raw_entry(b"x/.wh..wh..opq"), &root, false).unwrap();
        assert!(outer.path().join("host/passwd").exists());
    }

    #[test]
    fn dot_slash_whiteouts_still_apply() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        unpack_layer(layer(&[E::Dir("etc"), E::File("etc/drop", b"1")]), r, false).unwrap();
        unpack_layer(raw_entry(b"./etc/.wh.drop"), r, false).unwrap();
        assert!(!r.join("etc/drop").exists());
    }

    #[test]
    fn device_nodes_are_left_out() {
        let mut b = tar::Builder::new(tempfile::tempfile().unwrap());
        for (name, kind) in [("disk", tar::EntryType::Block), ("mem", tar::EntryType::Char)] {
            let mut h = header();
            h.set_entry_type(kind);
            h.set_mode(0o666);
            h.set_size(0);
            h.set_device_major(253).unwrap();
            h.set_device_minor(0).unwrap();
            b.append_data(&mut h, name, io::empty()).unwrap();
        }
        let mut h = header();
        h.set_mode(0o644);
        h.set_size(2);
        b.append_data(&mut h, "kept", &b"ok"[..]).unwrap();
        let mut f = b.into_inner().unwrap();
        f.rewind().unwrap();

        let root = tempfile::tempdir().unwrap();
        unpack_layer(f, root.path(), false).unwrap();
        assert!(fs::symlink_metadata(root.path().join("disk")).is_err());
        assert!(fs::symlink_metadata(root.path().join("mem")).is_err());
        assert_eq!(fs::read_to_string(root.path().join("kept")).unwrap(), "ok");
    }

    /// One pax record, `<len> <key>=<value>\n`, its length counting itself.
    fn pax_record(key: &str, value: &[u8]) -> Vec<u8> {
        let body = [key.as_bytes(), b"=", value, b"\n"].concat();
        let mut len = body.len() + 2;
        while format!("{len} ").len() + body.len() != len {
            len += 1;
        }
        [format!("{len} ").into_bytes(), body].concat()
    }

    #[test]
    fn only_harmless_xattrs_are_taken() {
        let records = [
            pax_record("SCHILY.xattr.security.capability", b"\x01\x00\x00\x02"),
            pax_record("SCHILY.xattr.user.mime", b"text/plain"),
            pax_record("SCHILY.xattr.user.overlay.opaque", b"y"),
            pax_record("SCHILY.xattr.trusted.overlay.redirect", b"/etc"),
            pax_record("SCHILY.xattr.security.selinux", b"system_u"),
        ]
        .concat();
        let mut b = tar::Builder::new(Vec::new());
        let mut x = header();
        x.set_entry_type(tar::EntryType::XHeader);
        x.set_size(records.len() as u64);
        b.append_data(&mut x, "pax", &records[..]).unwrap();
        let mut h = header();
        h.set_mode(0o755);
        h.set_size(0);
        b.append_data(&mut h, "bin/ping", io::empty()).unwrap();
        let data = b.into_inner().unwrap();

        let mut archive = tar::Archive::new(&data[..]);
        let mut entry = archive.entries().unwrap().next().unwrap().unwrap();
        let names: Vec<String> =
            xattrs_of(&mut entry).unwrap().into_iter().map(|(n, _)| n.into_string().unwrap()).collect();
        assert_eq!(names, ["security.capability", "user.mime"]);
    }

    #[test]
    fn owners_stay_within_a_berths_ids() {
        assert_eq!(owner_id(0), 0);
        assert_eq!(owner_id(65535), 65535);
        assert_eq!(owner_id(65536), 65534);
        assert_eq!(owner_id(1 << 40), 65534);
    }
}
