//! Mounts, including idmapped ones (open_tree, mount_setattr, move_mount),
//! which libc doesn't wrap yet.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

use super::{cstr, cvt};

pub fn mount(
    src: Option<&str>,
    target: &Path,
    fstype: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> io::Result<()> {
    let src = src.map(|s| CString::new(s).unwrap());
    let fstype = fstype.map(|s| CString::new(s).unwrap());
    let data = data.map(|s| CString::new(s).unwrap());
    let target = cstr(target);
    let r = unsafe {
        libc::mount(
            src.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
            target.as_ptr(),
            fstype.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
            flags,
            data.as_ref().map_or(std::ptr::null(), |s| s.as_ptr() as *const libc::c_void),
        )
    };
    cvt(r).map(drop).map_err(|e| io::Error::new(e.kind(), format!("mount {}: {e}", target.to_string_lossy())))
}

pub fn umount(target: &Path, flags: libc::c_int) -> io::Result<()> {
    let t = cstr(target);
    cvt(unsafe { libc::umount2(t.as_ptr(), flags) }).map(drop)
}

#[repr(C)]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}

const OPEN_TREE_CLONE: libc::c_uint = 1;
const MOUNT_ATTR_RDONLY: u64 = 0x1;
const MOUNT_ATTR_NOSUID: u64 = 0x2;
const MOUNT_ATTR_NODEV: u64 = 0x4;
const MOUNT_ATTR_NOEXEC: u64 = 0x8;
const MOUNT_ATTR_IDMAP: u64 = 0x0010_0000;
const MOVE_MOUNT_F_EMPTY_PATH: libc::c_uint = 0x4;

/// Bind-mounts src at target, read-only, with its owners shifted by the user
/// namespace userns: what host uid 0 owns appears owned by the namespace's root.
pub fn idmapped_bind(src: &Path, target: &Path, userns: RawFd) -> io::Result<()> {
    let tree = idmapped_tree(src, userns, true)?;
    let t = cstr(target);
    let empty = CString::new("").unwrap();
    let r = unsafe {
        libc::syscall(
            libc::SYS_move_mount,
            tree.as_raw_fd(),
            empty.as_ptr(),
            libc::AT_FDCWD,
            t.as_ptr(),
            MOVE_MOUNT_F_EMPTY_PATH,
        )
    };
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// A mount of src that's in no namespace's tree, reached only through the fd,
/// with its owners shifted by userns. Writable unless read_only; never
/// honouring devices, setuid bits or exec.
pub fn idmapped_tree(src: &Path, userns: RawFd, read_only: bool) -> io::Result<OwnedFd> {
    let s = cstr(src);
    let empty = CString::new("").unwrap();
    unsafe {
        let fd = libc::syscall(
            libc::SYS_open_tree,
            libc::AT_FDCWD,
            s.as_ptr(),
            OPEN_TREE_CLONE | libc::O_CLOEXEC as libc::c_uint,
        );
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let tree = OwnedFd::from_raw_fd(fd as RawFd);
        let mut set = MOUNT_ATTR_IDMAP;
        if read_only {
            set |= MOUNT_ATTR_RDONLY;
        } else {
            set |= MOUNT_ATTR_NOSUID | MOUNT_ATTR_NODEV | MOUNT_ATTR_NOEXEC;
        }
        let attr = MountAttr { attr_set: set, attr_clr: 0, propagation: 0, userns_fd: userns as u64 };
        let r = libc::syscall(
            libc::SYS_mount_setattr,
            tree.as_raw_fd(),
            empty.as_ptr(),
            libc::AT_EMPTY_PATH,
            &attr as *const MountAttr,
            std::mem::size_of::<MountAttr>(),
        );
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(tree)
    }
}
