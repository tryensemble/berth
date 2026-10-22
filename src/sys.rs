//! Thin wrappers over the Linux calls berth needs that std doesn't have.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;

pub fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r == -1 { Err(io::Error::last_os_error()) } else { Ok(r) }
}

pub fn cstr(p: &Path) -> CString {
    CString::new(p.as_os_str().as_bytes()).expect("path with a NUL byte")
}

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
const MOUNT_ATTR_IDMAP: u64 = 0x0010_0000;
const MOVE_MOUNT_F_EMPTY_PATH: libc::c_uint = 0x4;

/// Bind-mounts src at target, read-only, with its owners shifted by the user
/// namespace userns: what host uid 0 owns appears owned by the namespace's root.
pub fn idmapped_bind(src: &Path, target: &Path, userns: RawFd) -> io::Result<()> {
    let s = cstr(src);
    let t = cstr(target);
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
        let attr = MountAttr {
            attr_set: MOUNT_ATTR_IDMAP | MOUNT_ATTR_RDONLY,
            attr_clr: 0,
            propagation: 0,
            userns_fd: userns as u64,
        };
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
        let r = libc::syscall(
            libc::SYS_move_mount,
            tree.as_raw_fd(),
            empty.as_ptr(),
            libc::AT_FDCWD,
            t.as_ptr(),
            MOVE_MOUNT_F_EMPTY_PATH,
        );
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

pub fn setns(fd: RawFd, nstype: libc::c_int) -> io::Result<()> {
    cvt(unsafe { libc::setns(fd, nstype) }).map(drop)
}

pub fn pid_alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

/// Writes n in decimal into buf without allocating (safe between fork and exec).
pub fn itoa(mut n: u64, buf: &mut [u8; 24]) -> &[u8] {
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    &buf[i..]
}

/// Sends data with one file descriptor attached.
pub fn send_with_fd(sock: &UnixStream, data: &[u8], fd: RawFd) -> io::Result<()> {
    unsafe {
        let mut iov = libc::iovec { iov_base: data.as_ptr() as *mut _, iov_len: data.len() };
        let space = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as usize;
        let mut cbuf = vec![0u8; space];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr() as *mut _;
        msg.msg_controllen = space as _;
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(c) as *mut RawFd, fd);
        let n = libc::sendmsg(sock.as_raw_fd(), &msg, 0);
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if (n as usize) != data.len() {
            return Err(io::Error::other("short sendmsg"));
        }
    }
    Ok(())
}

/// Receives one message of at most 1 MB and the file descriptor attached to it.
pub fn recv_with_fd(sock: &UnixStream) -> io::Result<(Vec<u8>, Option<OwnedFd>)> {
    let mut buf = vec![0u8; 1 << 20];
    unsafe {
        let mut iov = libc::iovec { iov_base: buf.as_mut_ptr() as *mut _, iov_len: buf.len() };
        let space = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as usize;
        let mut cbuf = vec![0u8; space];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr() as *mut _;
        msg.msg_controllen = space as _;
        let n = libc::recvmsg(sock.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC);
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut fd = None;
        let c = libc::CMSG_FIRSTHDR(&msg);
        if !c.is_null() && (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
            let raw = std::ptr::read_unaligned(libc::CMSG_DATA(c) as *const RawFd);
            fd = Some(OwnedFd::from_raw_fd(raw));
        }
        buf.truncate(n as usize);
        Ok((buf, fd))
    }
}

/// Runs a host command, failing with its stderr when it fails.
pub fn run(cmd: &str, args: &[&str]) -> anyhow::Result<String> {
    let out = std::process::Command::new(cmd).args(args).output()?;
    if !out.status.success() {
        anyhow::bail!(
            "{cmd} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
