use std::collections::HashMap;
use std::process::{Child, Command};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Reaps every child (PID 1 inherits orphans) and hands exit codes to whoever
/// waits for that pid.
#[derive(Clone)]
pub struct Reaper(Arc<Mutex<HashMap<i32, mpsc::Sender<i32>>>>);

impl Reaper {
    pub fn start() -> Self {
        let r = Reaper(Arc::new(Mutex::new(HashMap::new())));
        let map = r.0.clone();
        std::thread::spawn(move || {
            loop {
                // Look without reaping first: a child whose exec failed is
                // reaped by spawn itself, under the table's lock.
                let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
                let r = unsafe { libc::waitid(libc::P_ALL, 0, &mut info, libc::WEXITED | libc::WNOWAIT) };
                if r < 0 {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                let pid = unsafe { info.si_pid() };
                let mut map = map.lock().unwrap();
                let mut status = 0;
                if unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } != pid {
                    continue;
                }
                let code =
                    if libc::WIFEXITED(status) { libc::WEXITSTATUS(status) } else { 128 + libc::WTERMSIG(status) };
                if let Some(tx) = map.remove(&pid) {
                    let _ = tx.send(code);
                }
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
