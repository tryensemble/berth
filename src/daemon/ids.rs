//! The host's user and group ids berth takes for itself, and keeping the rest
//! of the machine out of them.
//!
//! Every berth's files and processes belong to host ids in berth's range (its
//! own block of them, and the ids images are unpacked and mirrors fetched
//! as). A host user given ids in that range, as an account or as subordinate
//! ids for its own user namespaces (newuidmap), would own a berth's files and
//! could signal its processes. So the daemon won't start while any overlap,
//! and it records the range in /etc/subuid and /etc/subgid under the name
//! `berth`, so useradd hands out subordinate ids elsewhere.

use std::fs;
use std::io::{self, Write};
use std::ops::Range;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, bail};

use super::{UID_COUNT, UID_START, UNPACK_UID};
use crate::net::MAX_BERTHS;

/// Berths a machine holds unless BERTH_MAX_BERTHS says otherwise: plenty for
/// one machine, and small enough (ids up to about 269 million) to leave
/// useradd most of its own span for subordinate ids.
const DEFAULT_MAX_BERTHS: u32 = 4096;

/// The name berth's range is reserved under. The daemon won't start while an
/// account has it, so no one can map the range through newuidmap.
const OWNER: &str = "berth";

/// How many berths this machine may hold, which decides how many ids berth
/// takes: BERTH_MAX_BERTHS, or 4096. Read once; [`check_and_reserve`] has
/// refused a bad value by the time anything else asks.
pub fn max_berths() -> u32 {
    static MAX: OnceLock<u32> = OnceLock::new();
    *MAX.get_or_init(|| parse_max(std::env::var("BERTH_MAX_BERTHS").ok().as_deref()).unwrap_or(DEFAULT_MAX_BERTHS))
}

/// A BERTH_MAX_BERTHS value. Berth numbers start at 2 (the gateway has 1),
/// and the network has addresses for MAX_BERTHS.
fn parse_max(v: Option<&str>) -> anyhow::Result<u32> {
    let Some(v) = v else { return Ok(DEFAULT_MAX_BERTHS) };
    let n: u32 = v.trim().parse().with_context(|| format!("BERTH_MAX_BERTHS={v} isn't a number"))?;
    if !(3..=MAX_BERTHS).contains(&n) {
        bail!("BERTH_MAX_BERTHS={v} must be between 3 and {MAX_BERTHS}");
    }
    Ok(n)
}

/// Every host id berth uses: the unpack ids, the fetch id and every berth's
/// block.
pub fn range() -> Range<u64> {
    range_for(max_berths())
}

fn range_for(max_berths: u32) -> Range<u64> {
    UNPACK_UID as u64..UID_START as u64 + max_berths as u64 * UID_COUNT as u64
}

/// Refuses to go on while a host account or subordinate range overlaps
/// berth's ids, or an account could claim the reservation, then makes sure
/// the range is reserved.
pub fn check_and_reserve() -> anyhow::Result<()> {
    parse_max(std::env::var("BERTH_MAX_BERTHS").ok().as_deref())?;
    let r = range();
    let mut clashes = Vec::new();
    let passwd = read(Path::new("/etc/passwd"))?;
    if accounts(&passwd).any(|f| f.first() == Some(&OWNER.as_bytes())) {
        clashes.push(format!("/etc/passwd: an account named {OWNER}, which could map every berth's ids"));
    }
    // A user's own id and its primary group's.
    for field in [2, 3] {
        clashes.extend(overlaps("/etc/passwd", &passwd, Kind::Account(field), &r));
    }
    clashes.extend(overlaps("/etc/group", &read(Path::new("/etc/group"))?, Kind::Account(2), &r));
    for file in ["/etc/subuid", "/etc/subgid"] {
        clashes.extend(overlaps(file, &read(Path::new(file))?, Kind::Subordinate, &r));
    }
    if !clashes.is_empty() {
        bail!(
            "host ids overlap berth's ({}..{}), so their owners could reach berths' files and processes; \
             move them first:\n  {}",
            r.start,
            r.end,
            clashes.join("\n  ")
        );
    }
    for file in ["/etc/subuid", "/etc/subgid"] {
        reserve(Path::new(file), &r).with_context(|| format!("reserving berth's ids in {file}"))?;
    }
    Ok(())
}

/// A file's bytes; none if it isn't there. Any other failure is an error: a
/// file that can't be read can't be checked, nor safely rewritten.
fn read(file: &Path) -> anyhow::Result<Vec<u8>> {
    match fs::read(file) {
        Ok(b) => Ok(b),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", file.display())),
    }
}

/// The colon-separated fields of each line of text.
fn accounts(text: &[u8]) -> impl Iterator<Item = Vec<&[u8]>> {
    text.split(|&c| c == b'\n').map(|l| l.split(|&c| c == b':').collect())
}

#[derive(Clone, Copy)]
enum Kind {
    /// `name:x:id:...`, as in passwd and group, with the id in this field.
    Account(usize),
    /// `owner:start:count`, as in subuid and subgid.
    Subordinate,
}

fn number(field: Option<&&[u8]>) -> Option<u64> {
    std::str::from_utf8(field?).ok()?.parse().ok()
}

/// The lines of text (a file of kind) whose ids fall in r, as `file:line: text`.
fn overlaps(file: &str, text: &[u8], kind: Kind, r: &Range<u64>) -> Vec<String> {
    let mut v = Vec::new();
    for (i, f) in accounts(text).enumerate() {
        let ids = match kind {
            Kind::Account(field) => number(f.get(field)).map(|id| id..id.saturating_add(1)),
            Kind::Subordinate if f.first() == Some(&OWNER.as_bytes()) => None,
            Kind::Subordinate => match (number(f.get(1)), number(f.get(2))) {
                (Some(start), Some(count)) => Some(start..start.saturating_add(count)),
                _ => None,
            },
        };
        if let Some(ids) = ids
            && ids.start < r.end
            && r.start < ids.end
        {
            v.push(format!("{file}:{}: {}", i + 1, String::from_utf8_lossy(&f.join(&b':'))));
        }
    }
    v
}

/// The text of a subordinate-id file with berth's reservation for r in it,
/// or None if it's there already. Every other line is kept byte for byte.
fn with_reservation(text: &[u8], r: &Range<u64>) -> Option<Vec<u8>> {
    let line = format!("{OWNER}:{}:{}", r.start, r.end - r.start);
    let lines = text.split(|&c| c == b'\n').filter(|l| !l.is_empty());
    if lines.clone().any(|l| l == line.as_bytes()) {
        return None;
    }
    // An older reservation (for another range) gives way to this one.
    let mut out = Vec::new();
    for l in lines.filter(|l| l.split(|&c| c == b':').next() != Some(OWNER.as_bytes())) {
        out.extend_from_slice(l);
        out.push(b'\n');
    }
    out.extend_from_slice(line.as_bytes());
    out.push(b'\n');
    Some(out)
}

/// Writes berth's reservation into file, as shadow's own tools do: under its
/// lock (or not at all while another tool holds it), keeping the old file as
/// `file-` and the new one's owner and mode as the old one's, and replacing
/// it whole, so a reader never sees half of it.
fn reserve(file: &Path, r: &Range<u64>) -> anyhow::Result<()> {
    // Where a link there points, so the link stays.
    let file = fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
    let text = read(&file)?;
    let Some(new) = with_reservation(&text, r) else { return Ok(()) };
    let Some(_lock) = Lock::take(&file)? else {
        eprintln!("berthd: {} is locked by another tool; berth's ids aren't reserved in it yet", file.display());
        return Ok(());
    };
    let (mode, uid, gid) = match fs::metadata(&file) {
        Ok(m) => (m.mode() & 0o7777, m.uid(), m.gid()),
        Err(_) => (0o644, 0, 0),
    };
    if !text.is_empty() {
        fs::write(sibling(&file, "-"), &text)?;
    }
    let tmp = sibling(&file, ".berth-tmp");
    let mut f = fs::File::create(&tmp)?;
    f.set_permissions(fs::Permissions::from_mode(mode))?;
    std::os::unix::fs::fchown(&f, Some(uid), Some(gid))?;
    f.write_all(&new)?;
    f.sync_all()?;
    fs::rename(&tmp, &file)?;
    eprintln!("berthd: reserved ids {}..{} for berth in {}", r.start, r.end, file.display());
    Ok(())
}

/// file with suffix added to its name.
fn sibling(file: &Path, suffix: &str) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// shadow's lock on a file: `file.lock`, made by linking a file holding our
/// pid, so whoever makes it first has it.
struct Lock(PathBuf);

impl Lock {
    fn take(file: &Path) -> anyhow::Result<Option<Lock>> {
        let lock = sibling(file, ".lock");
        let mine = sibling(file, &format!(".{}", std::process::id()));
        fs::write(&mine, format!("{}", std::process::id()))?;
        let linked = fs::hard_link(&mine, &lock);
        let _ = fs::remove_file(&mine);
        match linked {
            Ok(()) => Ok(Some(Lock(lock))),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: Range<u64> = 1_000..2_000;

    #[test]
    fn the_range_covers_unpacking_fetching_and_every_berth() {
        let r = range_for(DEFAULT_MAX_BERTHS);
        assert!(r.contains(&(UNPACK_UID as u64)) && r.contains(&(super::super::repo::FETCH_UID as u64)));
        // The last berth's last id.
        assert!(r.contains(&(UID_START as u64 + (DEFAULT_MAX_BERTHS as u64 - 1) * 65_536 + 65_535)));
        assert_eq!(r.end, 1_000_000 + 4096 * 65_536);
        // Well inside useradd's default span (to 600100000), leaving it room.
        assert!(r.end < 300_000_000);
        assert!(range_for(MAX_BERTHS).end <= u32::MAX as u64, "every id fits in a uid_t");
    }

    #[test]
    fn the_berth_count_is_a_setting_within_bounds() {
        assert_eq!(parse_max(None).unwrap(), DEFAULT_MAX_BERTHS);
        assert_eq!(parse_max(Some("64")).unwrap(), 64);
        for bad in ["0", "2", "65001", "many", "-5"] {
            assert!(parse_max(Some(bad)).is_err(), "{bad}");
        }
        assert_eq!(range_for(64).end, 1_000_000 + 64 * 65_536);
    }

    #[test]
    fn subordinate_ranges_that_touch_berths_are_found() {
        let text = "alice:100:900\nbob:900:200\ncarol:1999:5\ndave:2000:10\nberth:1000:1000\nbroken\n";
        let found = overlaps("/etc/subuid", text.as_bytes(), Kind::Subordinate, &R);
        assert_eq!(found, ["/etc/subuid:2: bob:900:200", "/etc/subuid:3: carol:1999:5"]);
    }

    #[test]
    fn accounts_with_berth_ids_are_found() {
        let text =
            b"root:x:0:0::/root:/bin/sh\nldap:x:1500:1500::/home/ldap:/bin/sh\nweb:x:30:1700:\xe9t\xe9:/:/bin/sh\n";
        assert_eq!(
            overlaps("/etc/passwd", text, Kind::Account(2), &R),
            ["/etc/passwd:2: ldap:x:1500:1500::/home/ldap:/bin/sh"]
        );
        // A primary group in the range counts too, and odd bytes don't hide it.
        assert_eq!(overlaps("/etc/passwd", text, Kind::Account(3), &R).len(), 2);
    }

    #[test]
    fn the_reservation_is_added_once_and_replaces_an_old_one() {
        let added = with_reservation(b"alice:100:65536\n", &R).unwrap();
        assert_eq!(added, b"alice:100:65536\nberth:1000:1000\n");
        assert_eq!(with_reservation(&added, &R), None);
        let moved = with_reservation(b"berth:5:5\nalice:100:65536", &R).unwrap();
        assert_eq!(moved, b"alice:100:65536\nberth:1000:1000\n");
    }

    #[test]
    fn huge_ranges_dont_wrap_around() {
        let text = format!("big:1:{}\n", u64::MAX);
        assert_eq!(overlaps("/etc/subuid", text.as_bytes(), Kind::Subordinate, &R).len(), 1);
    }

    #[test]
    fn reserving_keeps_the_file_its_mode_and_a_backup() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("subuid");
        fs::write(&file, b"alice:100:65536\n\xffodd:200:5\n").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
        reserve(&file, &R).unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"alice:100:65536\n\xffodd:200:5\nberth:1000:1000\n");
        assert_eq!(fs::metadata(&file).unwrap().mode() & 0o777, 0o640);
        assert_eq!(fs::read(sibling(&file, "-")).unwrap(), b"alice:100:65536\n\xffodd:200:5\n");
        assert!(!sibling(&file, ".lock").exists());
        // While another tool holds the lock, the file is left alone.
        fs::write(&file, b"alice:100:65536\n").unwrap();
        fs::write(sibling(&file, ".lock"), b"1").unwrap();
        reserve(&file, &R).unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"alice:100:65536\n");
    }
}
