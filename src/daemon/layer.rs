//! Layers: what a berth installed, kept read-only for other berths to build on.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, lchown};
use std::path::Path;

use super::berth::Berth;
use super::{UID_COUNT, paths};
use crate::sys::beneath;

/// What berth itself puts in a berth, which a layer mustn't keep.
const BERTHS_OWN: &[&str] = &["workspace", "berth", "var/log/berth", "etc/resolv.conf", "etc/hostname", "etc/hosts"];

/// Turns a stopped berth's upper layer into the layer name.
pub fn commit(b: &Berth, name: &str) -> anyhow::Result<()> {
    let upper = b.dir().join("upper");
    // The berth's files are its own: nothing here may follow its links.
    for p in BERTHS_OWN {
        beneath::remove(&upper, Path::new(p))?;
    }
    shift_owners(&upper, b.uid_base)?;
    let target = paths::state("layers").join(name);
    if target.exists() {
        fs::remove_dir_all(&target)?;
    }
    fs::rename(&upper, &target)?;
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// Moves files a berth owned (ids base..base+65536) back to ids from 0, so the
/// layer can be shifted into any other berth's range.
pub fn shift_owners(p: &Path, base: u32) -> anyhow::Result<()> {
    shift(p, |id| if (base..base + UID_COUNT).contains(&id) { id - base } else { id })
}

fn shift(p: &Path, f: impl Fn(u32) -> u32 + Copy) -> anyhow::Result<()> {
    let m = fs::symlink_metadata(p)?;
    let (uid, gid) = (f(m.uid()), f(m.gid()));
    if (uid, gid) != (m.uid(), m.gid()) {
        lchown(p, Some(uid), Some(gid))?;
        // chown clears setuid bits; put the mode back (links have none).
        if !m.file_type().is_symlink() {
            fs::set_permissions(p, fs::Permissions::from_mode(m.mode() & 0o7777))?;
        }
    }
    if m.is_dir() {
        for e in fs::read_dir(p)? {
            shift(&e?.path(), f)?;
        }
    }
    Ok(())
}
