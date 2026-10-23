//! Working on paths inside a tree someone else controls, such as a berth's
//! own files, without following its symlinks out of it.
//!
//! The tree must not change while these run: they're used while a berth is
//! being built or after it stopped, when nothing in it runs. Each walks the
//! path one component at a time without following links.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

fn components(rel: &Path) -> io::Result<Vec<&std::ffi::OsStr>> {
    rel.components()
        .filter(|c| !matches!(c, Component::RootDir | Component::CurDir))
        .map(|c| match c {
            Component::Normal(n) => Ok(n),
            _ => Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{} leaves its tree", rel.display()))),
        })
        .collect()
}

/// base/rel as a real directory, made where missing. A component that's a
/// symlink or a file is an error, or, with replace, is removed and made a
/// directory: for mount points that belong to berth, not to what's in it.
pub fn ensure_dir(base: &Path, rel: &Path, replace: bool) -> io::Result<PathBuf> {
    let mut p = base.to_path_buf();
    for c in components(rel)? {
        p.push(c);
        match fs::symlink_metadata(&p) {
            Ok(m) if m.is_dir() => continue,
            Ok(_) if replace => fs::remove_file(&p)?,
            Ok(_) => {
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{} isn't a directory", p.display())));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        fs::create_dir(&p)?;
    }
    Ok(p)
}

/// The directory base/rel if every component of it is a real directory.
pub fn dir(base: &Path, rel: &Path) -> io::Result<Option<PathBuf>> {
    let mut p = base.to_path_buf();
    for c in components(rel)? {
        p.push(c);
        match fs::symlink_metadata(&p) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Ok(None),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        }
    }
    Ok(Some(p))
}

/// Replaces base/rel with a new regular file holding data. Its directory must
/// be real; whatever was at rel (a file, a symlink) is removed first.
pub fn write(base: &Path, rel: &Path, data: &[u8]) -> io::Result<()> {
    let parent = rel.parent().unwrap_or(Path::new(""));
    let name = rel.file_name().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    let d = ensure_dir(base, parent, false)?;
    let p = d.join(name);
    match fs::symlink_metadata(&p) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(&p)?,
        Ok(_) => fs::remove_file(&p)?,
        Err(_) => {}
    }
    let mut f: File =
        OpenOptions::new().write(true).create_new(true).mode(0o644).custom_flags(libc::O_NOFOLLOW).open(&p)?;
    f.write_all(data)
}

/// Removes base/rel, whatever it is, if it's there and inside base. A link on
/// the way means it isn't, and nothing is removed.
pub fn remove(base: &Path, rel: &Path) -> io::Result<()> {
    let parent = rel.parent().unwrap_or(Path::new(""));
    let Some(name) = rel.file_name() else { return Ok(()) };
    let Some(d) = dir(base, parent)? else { return Ok(()) };
    let p = d.join(name);
    match fs::symlink_metadata(&p) {
        // remove_dir_all doesn't follow links inside.
        Ok(m) if m.is_dir() => fs::remove_dir_all(&p),
        Ok(_) => fs::remove_file(&p),
        Err(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct Tree {
        _dir: tempfile::TempDir,
        base: PathBuf,
        outside: PathBuf,
    }

    /// A tree whose `etc` links to a directory outside it.
    fn tree() -> Tree {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&base).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("passwd"), "keep").unwrap();
        symlink(&outside, base.join("etc")).unwrap();
        Tree { _dir: dir, base, outside }
    }

    #[test]
    fn writes_never_follow_links_out() {
        let t = tree();
        assert!(write(&t.base, Path::new("etc/passwd"), b"x").is_err());
        assert_eq!(fs::read_to_string(t.outside.join("passwd")).unwrap(), "keep");

        fs::create_dir(t.base.join("real")).unwrap();
        symlink(t.outside.join("passwd"), t.base.join("real/file")).unwrap();
        write(&t.base, Path::new("real/file"), b"mine").unwrap();
        assert_eq!(fs::read_to_string(t.base.join("real/file")).unwrap(), "mine");
        assert!(!fs::symlink_metadata(t.base.join("real/file")).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(t.outside.join("passwd")).unwrap(), "keep");
    }

    #[test]
    fn removals_never_follow_links_out() {
        let t = tree();
        remove(&t.base, Path::new("etc/passwd")).unwrap();
        assert!(t.outside.join("passwd").exists());
        remove(&t.base, Path::new("etc")).unwrap();
        assert!(!t.base.join("etc").exists());
        assert!(t.outside.exists());
    }

    #[test]
    fn mount_points_are_made_real() {
        let t = tree();
        assert!(ensure_dir(&t.base, Path::new("etc/x"), false).is_err());
        let p = ensure_dir(&t.base, Path::new("/etc/x"), true).unwrap();
        assert_eq!(p, t.base.join("etc/x"));
        assert!(fs::symlink_metadata(t.base.join("etc")).unwrap().is_dir());
        assert!(!t.outside.join("x").exists());
    }

    #[test]
    fn paths_that_climb_out_are_refused() {
        let t = tree();
        assert!(ensure_dir(&t.base, Path::new("../x"), true).is_err());
        assert!(write(&t.base, Path::new("a/../../x"), b"").is_err());
        assert_eq!(dir(&t.base, Path::new("etc")).unwrap(), None);
    }
}
