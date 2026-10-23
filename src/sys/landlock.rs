//! Landlock: confining what the calling thread (and what it starts) may write.
//!
//! The daemon does some of its work as host root over what strangers send:
//! downloading images (unpacking them happens in a process of its own) and
//! fetching repositories. That work runs in a thread that first gives up
//! writing anywhere but its own target directory, so a bug in it can't touch
//! the rest of the machine.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

const CREATE_RULESET_VERSION: u32 = 1;
const RULE_PATH_BENEATH: libc::c_int = 1;

const WRITE_FILE: u64 = 1 << 1;
const REMOVE_DIR: u64 = 1 << 4;
const REMOVE_FILE: u64 = 1 << 5;
const MAKE_CHAR: u64 = 1 << 6;
const MAKE_DIR: u64 = 1 << 7;
const MAKE_REG: u64 = 1 << 8;
const MAKE_SOCK: u64 = 1 << 9;
const MAKE_FIFO: u64 = 1 << 10;
const MAKE_BLOCK: u64 = 1 << 11;
const MAKE_SYM: u64 = 1 << 12;
/// ABI 2: linking or renaming across directories.
const REFER: u64 = 1 << 13;
/// ABI 3.
const TRUNCATE: u64 = 1 << 14;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// The Landlock ABI the kernel speaks, or None without Landlock.
pub fn abi() -> Option<u32> {
    let r = unsafe {
        libc::syscall(libc::SYS_landlock_create_ruleset, std::ptr::null::<RulesetAttr>(), 0, CREATE_RULESET_VERSION)
    };
    (r > 0).then_some(r as u32)
}

fn writes(abi: u32) -> u64 {
    let mut a = WRITE_FILE
        | REMOVE_DIR
        | REMOVE_FILE
        | MAKE_CHAR
        | MAKE_DIR
        | MAKE_REG
        | MAKE_SOCK
        | MAKE_FIFO
        | MAKE_BLOCK
        | MAKE_SYM;
    if abi >= 2 {
        a |= REFER;
    }
    if abi >= 3 {
        a |= TRUNCATE;
    }
    a
}

/// A set of directories writes are allowed beneath, ready to enforce.
pub struct Ruleset(OwnedFd);

impl Ruleset {
    /// None when the kernel has no Landlock.
    pub fn new(dirs: &[&Path]) -> io::Result<Option<Ruleset>> {
        let Some(abi) = abi() else { return Ok(None) };
        let handled = writes(abi);
        let attr = RulesetAttr { handled_access_fs: handled };
        let fd = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &attr as *const RulesetAttr,
                std::mem::size_of::<RulesetAttr>(),
                0,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let ruleset = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        for dir in dirs {
            let c = CString::new(dir.as_os_str().as_bytes()).map_err(io::Error::other)?;
            let d = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
            if d < 0 {
                return Err(io::Error::last_os_error());
            }
            let d = unsafe { OwnedFd::from_raw_fd(d) };
            // A file (/dev/null, say) only takes the rights files have.
            let is_dir = std::fs::metadata(dir)?.is_dir();
            let allowed = if is_dir { handled } else { handled & (WRITE_FILE | TRUNCATE) };
            let rule = PathBeneathAttr { allowed_access: allowed, parent_fd: d.as_raw_fd() };
            let r = unsafe {
                libc::syscall(
                    libc::SYS_landlock_add_rule,
                    ruleset.as_raw_fd(),
                    RULE_PATH_BENEATH,
                    &rule as *const PathBeneathAttr,
                    0,
                )
            };
            if r < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Some(Ruleset(ruleset)))
    }

    /// Confines the calling thread and what it starts from now on. Doesn't
    /// allocate, so it's safe between fork and exec. Without CAP_SYS_ADMIN,
    /// set no_new_privs first.
    pub fn restrict_self(&self) -> io::Result<()> {
        if unsafe { libc::syscall(libc::SYS_landlock_restrict_self, self.0.as_raw_fd(), 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Lets the calling thread, and whatever it starts, write only beneath dirs.
/// Reading stays as it was. Returns false, changing nothing, when the kernel
/// has no Landlock.
pub fn restrict_writes_to(dirs: &[&Path]) -> io::Result<bool> {
    let Some(ruleset) = Ruleset::new(dirs)? else { return Ok(false) };
    if unsafe { libc::geteuid() } != 0 && unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    ruleset.restrict_self()?;
    Ok(true)
}

/// Runs f in a new thread that may write only beneath dirs.
pub fn confined<T: Send + 'static>(
    dirs: &[&Path],
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    let dirs: Vec<std::path::PathBuf> = dirs.iter().map(|d| d.to_path_buf()).collect();
    std::thread::spawn(move || {
        let refs: Vec<&Path> = dirs.iter().map(|d| d.as_path()).collect();
        if !restrict_writes_to(&refs)? {
            warn_once();
        }
        f()
    })
    .join()
    .map_err(|_| anyhow::anyhow!("a confined task panicked"))?
}

pub fn warn_once() {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| eprintln!("berthd: this kernel has no Landlock; image and repository work runs unconfined"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_outside_the_allowed_dirs_fail() {
        if abi().is_none() {
            eprintln!("no Landlock on this kernel; skipping");
            return;
        }
        let allowed = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let (a, o) = (allowed.path().to_path_buf(), other.path().to_path_buf());
        let result = confined(&[allowed.path()], move || {
            std::fs::write(a.join("inside"), "ok")?;
            let outside = std::fs::write(o.join("outside"), "no");
            let read = std::fs::read_dir("/").map(|_| ());
            Ok((outside.is_err(), read.is_ok()))
        })
        .unwrap();
        assert_eq!(result, (true, true));
        assert!(allowed.path().join("inside").exists());
        assert!(!other.path().join("outside").exists());
        // The confinement ended with its thread.
        std::fs::write(other.path().join("later"), "fine").unwrap();
    }
}
