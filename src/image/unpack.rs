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
        cmd.pre_exec(move || {
            sys::cvt(libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL))?;
            sys::setns(u, libc::CLONE_NEWUSER)?;
            // Become the namespace's root (the first unpack id): host root
            // isn't mapped in it, and an unmapped uid execs without caps.
            sys::cvt(libc::setgroups(0, std::ptr::null()))?;
            sys::cvt(libc::setresgid(0, 0, 0))?;
            sys::cvt(libc::setresuid(0, 0, 0))?;
            sys::cvt(libc::unshare(libc::CLONE_NEWNET))?;
            // The tree's fd, kept across exec for this child alone.
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
    archive.set_preserve_ownerships(true);
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
        let xattrs = xattrs_of(&mut entry)?;
        if entry.unpack_in(root)? {
            set_xattrs(&root.join(&path), &xattrs);
        }
    }
    Ok(())
}

/// The extended attributes an entry carries that an image may set: file
/// capabilities (ping's, say) and the user namespace's own. Not trusted.*,
/// which overlayfs reads its own state from, nor security labels.
fn xattrs_of<R: Read>(entry: &mut tar::Entry<R>) -> io::Result<Vec<(CString, Vec<u8>)>> {
    let mut v = Vec::new();
    let Some(exts) = entry.pax_extensions()? else { return Ok(v) };
    for ext in exts {
        let ext = ext?;
        let Some(name) = ext.key_bytes().strip_prefix(b"SCHILY.xattr.") else { continue };
        if (name == b"security.capability" || name.starts_with(b"user."))
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
}
