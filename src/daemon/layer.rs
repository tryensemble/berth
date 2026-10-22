//! Layers: what a berth installed, kept read-only for other berths to build on.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, lchown};
use std::path::Path;

use anyhow::bail;

use super::{Daemon, UID_COUNT, paths, valid_name};

impl Daemon {
    /// Keeps what a berth installed (its upper layer) as a read-only layer
    /// other berths can be made from, and removes the berth.
    pub fn commit_layer(&self, id: &str, name: &str) -> anyhow::Result<()> {
        if !valid_name(name) {
            bail!("bad layer name {name}");
        }
        let b = self.get(id)?;
        self.stop(&b);
        let upper = b.dir().join("upper");
        for p in ["workspace", "berth", "var/log/berth", "etc/resolv.conf", "etc/hostname", "etc/hosts"] {
            let p = upper.join(p);
            let _ = if p.is_dir() { fs::remove_dir_all(&p) } else { fs::remove_file(&p) };
        }
        shift_owners(&upper, b.uid_base)?;
        let target = paths::state("layers").join(name);
        if target.exists() {
            fs::remove_dir_all(&target)?;
        }
        fs::rename(&upper, &target)?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755))?;
        self.destroy(id)
    }
}

/// Moves files a berth owned (ids base..base+65536) back to ids from 0, so the
/// layer can be shifted into any other berth's range.
pub fn shift_owners(p: &Path, base: u32) -> anyhow::Result<()> {
    let m = fs::symlink_metadata(p)?;
    let shift = |id: u32| if (base..base + UID_COUNT).contains(&id) { id - base } else { id };
    let (uid, gid) = (shift(m.uid()), shift(m.gid()));
    if (uid, gid) != (m.uid(), m.gid()) {
        // chown clears setuid bits; put the mode back.
        lchown(p, Some(uid), Some(gid))?;
        if !m.file_type().is_symlink() {
            fs::set_permissions(p, fs::Permissions::from_mode(m.mode() & 0o7777))?;
        }
    }
    if m.is_dir() {
        for e in fs::read_dir(p)? {
            shift_owners(&e?.path(), base)?;
        }
    }
    Ok(())
}
