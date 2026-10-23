//! Unpacking image layers, with OCI whiteouts.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

/// Unpacks a layer tarball over root. `.wh.<name>` removes name from the
/// layers below; `.wh..wh..opq` empties its directory first. Device nodes are
/// left out: a berth gets its devices from its own /dev, never from an image.
pub fn unpack_layer(file: File, root: &Path, gzip: bool) -> anyhow::Result<()> {
    let reader: Box<dyn Read> = if gzip { Box::new(flate2::read::GzDecoder::new(file)) } else { Box::new(file) };
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    archive.set_preserve_ownerships(true);
    archive.set_unpack_xattrs(true);
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
        entry.unpack_in(root)?;
    }
    Ok(())
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
