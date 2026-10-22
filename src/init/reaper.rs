use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::{Child, Command};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

/// Reaps every child (PID 1 inherits orphans) and hands exit codes to whoever
/// waits for that pid. SIGCHLD arrives on a signalfd, so there's no polling
/// and a command that exits is reaped at once.
#[derive(Clone)]
pub struct Reaper(Arc<Mutex<HashMap<i32, mpsc::Sender<i32>>>>);

impl Reaper {
    pub fn start() -> Self {
        let r = Reaper(Arc::new(Mutex::new(HashMap::new())));
        let fd = block_sigchld();
        let map = r.0.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; std::mem::size_of::<libc::signalfd_siginfo>()];
            loop {
                // Blocks until a child's state changes.
                let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr() as *mut _, buf.len()) };
                if n < 0 {
                    let e = std::io::Error::last_os_error();
                    if e.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    // Don't let the reaper die, or zombies would pile up; a
                    // short pause keeps an unexpected persistent error from
                    // spinning. reap() below still runs, in case a child is
                    // waiting.
                    eprintln!("berth-init: reaper read: {e}");
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                reap(&map);
            }
        });
        r
    }

    /// Spawns cmd while holding the table, so its exit can't be reaped before
    /// it's registered.
    pub fn spawn(&self, cmd: &mut Command) -> std::io::Result<(Child, mpsc::Receiver<i32>)> {
        let mut map = self.0.lock().unwrap();
        let child = cmd.spawn()?;
        let (tx, rx) = mpsc::channel();
        map.insert(child.id() as i32, tx);
        Ok((child, rx))
    }
}

/// Reaps every child that's ready and delivers its code. Holds the table, so a
/// spawn in progress finishes registering first.
fn reap(map: &Mutex<HashMap<i32, mpsc::Sender<i32>>>) {
    let mut map = map.lock().unwrap();
    loop {
        let mut status = 0;
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid <= 0 {
            return;
        }
        let code = if libc::WIFEXITED(status) { libc::WEXITSTATUS(status) } else { 128 + libc::WTERMSIG(status) };
        if let Some(tx) = map.remove(&pid) {
            let _ = tx.send(code);
        }
    }
}

/// Blocks SIGCHLD in this thread (new threads inherit the mask) and returns a
/// signalfd it's delivered on instead.
fn block_sigchld() -> OwnedFd {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGCHLD);
        libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        let fd = libc::signalfd(-1, &set, libc::SFD_CLOEXEC);
        assert!(fd >= 0, "signalfd: {}", std::io::Error::last_os_error());
        OwnedFd::from_raw_fd(fd)
    }
}
