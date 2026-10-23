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
use std::io::Write;
use std::ops::Range;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::bail;

use super::{UID_COUNT, UID_START, UNPACK_UID};
use crate::net::MAX_BERTHS;

/// The name berth's range is reserved under. No account has it, so no one can
/// map the range through newuidmap.
const OWNER: &str = "berth";

/// Every host id berth uses: the unpack ids, the fetch id and every berth's
/// block.
pub fn range() -> Range<u64> {
    UNPACK_UID as u64..UID_START as u64 + MAX_BERTHS as u64 * UID_COUNT as u64
}

/// Refuses to go on while a host account or subordinate range overlaps
/// berth's ids, then makes sure the range is reserved.
pub fn check_and_reserve() -> anyhow::Result<()> {
    let r = range();
    let mut clashes = Vec::new();
    for (file, kind) in [("/etc/passwd", Kind::Account), ("/etc/group", Kind::Account)] {
        clashes.extend(overlaps(file, &read(file), kind, &r));
    }
    for file in ["/etc/subuid", "/etc/subgid"] {
        clashes.extend(overlaps(file, &read(file), Kind::Subordinate, &r));
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
        reserve(Path::new(file), &r)?;
    }
    Ok(())
}

fn read(file: &str) -> String {
    fs::read_to_string(file).unwrap_or_default()
}

#[derive(Clone, Copy)]
enum Kind {
    /// `name:x:id:...`, as in passwd and group.
    Account,
    /// `owner:start:count`, as in subuid and subgid.
    Subordinate,
}

/// The lines of text (a file of kind) whose ids fall in r, as `file:line: text`.
fn overlaps(file: &str, text: &str, kind: Kind, r: &Range<u64>) -> Vec<String> {
    let mut v = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let f: Vec<&str> = line.split(':').collect();
        let ids = match kind {
            Kind::Account => f.get(2).and_then(|id| id.parse::<u64>().ok()).map(|id| id..id + 1),
            Kind::Subordinate if f.first() == Some(&OWNER) => None,
            Kind::Subordinate => {
                match (f.get(1).and_then(|s| s.parse::<u64>().ok()), f.get(2).and_then(|c| c.parse::<u64>().ok())) {
                    (Some(start), Some(count)) => Some(start..start + count),
                    _ => None,
                }
            }
        };
        if let Some(ids) = ids
            && ids.start < r.end
            && r.start < ids.end
        {
            v.push(format!("{file}:{}: {line}", i + 1));
        }
    }
    v
}

/// The text of a subordinate-id file with berth's reservation for r in it,
/// or None if it's there already.
fn with_reservation(text: &str, r: &Range<u64>) -> Option<String> {
    let line = format!("{OWNER}:{}:{}", r.start, r.end - r.start);
    if text.lines().any(|l| l == line) {
        return None;
    }
    // An older reservation (for another range) gives way to this one.
    let mut out: String =
        text.lines().filter(|l| l.split(':').next() != Some(OWNER)).map(|l| format!("{l}\n")).collect();
    out.push_str(&line);
    out.push('\n');
    Some(out)
}

/// Writes berth's reservation into file, replacing the file whole so a reader
/// never sees half of it.
fn reserve(file: &Path, r: &Range<u64>) -> anyhow::Result<()> {
    let text = fs::read_to_string(file).unwrap_or_default();
    let Some(new) = with_reservation(&text, r) else { return Ok(()) };
    let tmp = file.with_extension("berth-tmp");
    let mut f = fs::File::create(&tmp)?;
    f.set_permissions(fs::Permissions::from_mode(0o644))?;
    f.write_all(new.as_bytes())?;
    f.sync_all()?;
    fs::rename(&tmp, file)?;
    eprintln!("berthd: reserved ids {}..{} for berth in {}", r.start, r.end, file.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: Range<u64> = 1_000..2_000;

    #[test]
    fn the_range_covers_unpacking_fetching_and_every_berth() {
        let r = range();
        assert!(r.contains(&(UNPACK_UID as u64)) && r.contains(&(super::super::repo::FETCH_UID as u64)));
        assert_eq!(r.end, 1_000_000 + 65_000 * 65_536);
        assert!(r.end <= u32::MAX as u64, "every id fits in a uid_t");
    }

    #[test]
    fn subordinate_ranges_that_touch_berths_are_found() {
        let text = "alice:100:900\nbob:900:200\ncarol:1999:5\ndave:2000:10\nberth:1000:1000\nbroken\n";
        let found = overlaps("/etc/subuid", text, Kind::Subordinate, &R);
        assert_eq!(found, ["/etc/subuid:2: bob:900:200", "/etc/subuid:3: carol:1999:5"]);
    }

    #[test]
    fn accounts_with_berth_ids_are_found() {
        let text = "root:x:0:0::/root:/bin/sh\nldap:x:1500:1500::/home/ldap:/bin/sh\nnobody:x:65534:65534:::\n";
        assert_eq!(
            overlaps("/etc/passwd", text, Kind::Account, &R),
            ["/etc/passwd:2: ldap:x:1500:1500::/home/ldap:/bin/sh"]
        );
    }

    #[test]
    fn the_reservation_is_added_once_and_replaces_an_old_one() {
        let added = with_reservation("alice:100:65536\n", &R).unwrap();
        assert_eq!(added, "alice:100:65536\nberth:1000:1000\n");
        assert_eq!(with_reservation(&added, &R), None);
        let moved = with_reservation("berth:5:5\nalice:100:65536", &R).unwrap();
        assert_eq!(moved, "alice:100:65536\nberth:1000:1000\n");
    }
}
