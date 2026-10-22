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
                let mut status = 0;
                let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
                if pid < 0 {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                let code =
                    if libc::WIFEXITED(status) { libc::WEXITSTATUS(status) } else { 128 + libc::WTERMSIG(status) };
                if let Some(tx) = map.lock().unwrap().remove(&pid) {
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
