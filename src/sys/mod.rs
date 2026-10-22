//! Thin wrappers over the Linux calls berth needs that std doesn't have.

mod fd;
mod mount;

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub use fd::{recv_with_fd, send_with_fd, setns};
pub use mount::{idmapped_bind, mount, umount};

/// Turns a libc `-1` into the last OS error.
pub fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r == -1 { Err(io::Error::last_os_error()) } else { Ok(r) }
}

pub fn cstr(p: &Path) -> CString {
    CString::new(p.as_os_str().as_bytes()).expect("path with a NUL byte")
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

/// Runs a host command, failing with its stderr when it fails.
pub fn run(cmd: &str, args: &[&str]) -> anyhow::Result<String> {
    let out = std::process::Command::new(cmd).args(args).output()?;
    if !out.status.success() {
        anyhow::bail!("{cmd} {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn itoa_writes_decimal() {
        let mut buf = [0u8; 24];
        assert_eq!(itoa(0, &mut buf), b"0");
        assert_eq!(itoa(1234567, &mut buf), b"1234567");
        assert_eq!(itoa(u64::MAX, &mut buf), u64::MAX.to_string().as_bytes());
    }

    #[test]
    fn run_returns_stdout_or_stderr() {
        assert_eq!(run("sh", &["-c", "echo out"]).unwrap(), "out\n");
        let e = run("sh", &["-c", "echo bad >&2; exit 3"]).unwrap_err();
        assert!(e.to_string().ends_with(": bad"), "{e}");
    }

    #[test]
    fn pid_alive_knows_this_process() {
        assert!(pid_alive(std::process::id() as i32));
        assert!(!pid_alive(0));
        assert!(!pid_alive(-1));
    }
}
