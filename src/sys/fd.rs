//! Passing file descriptors over Unix sockets, and entering namespaces.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use super::cvt;

pub fn setns(fd: RawFd, nstype: libc::c_int) -> io::Result<()> {
    cvt(unsafe { libc::setns(fd, nstype) }).map(drop)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn a_descriptor_crosses_a_socket() {
        let (a, b) = UnixStream::pair().unwrap();
        let (mut mine, theirs) = UnixStream::pair().unwrap();
        send_with_fd(&a, b"spec", theirs.as_raw_fd()).unwrap();
        drop(theirs);

        let (msg, fd) = recv_with_fd(&b).unwrap();
        assert_eq!(msg, b"spec");
        let mut received = UnixStream::from(fd.expect("a descriptor"));
        received.write_all(b"through").unwrap();
        drop(received);
        let mut got = String::new();
        mine.read_to_string(&mut got).unwrap();
        assert_eq!(got, "through");
    }

    #[test]
    fn a_message_without_a_descriptor_has_none() {
        let (mut a, b) = UnixStream::pair().unwrap();
        a.write_all(b"plain").unwrap();
        let (msg, fd) = recv_with_fd(&b).unwrap();
        assert_eq!(msg, b"plain");
        assert!(fd.is_none());
    }
}
