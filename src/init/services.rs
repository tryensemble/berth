//! Services: detached commands init keeps running. One that exits is started
//! again after a pause that grows while it keeps failing.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::exec;
use super::reaper::Reaper;
use crate::api::ExecSpec;

/// A run longer than this resets the pause before the next restart.
const HEALTHY: Duration = Duration::from_secs(30);
const FIRST_PAUSE: Duration = Duration::from_millis(500);
const MAX_PAUSE: Duration = Duration::from_secs(30);
/// How long a stopped service gets to end after SIGTERM before SIGKILL.
const GRACE: Duration = Duration::from_secs(5);

struct Service {
    /// Bumped whenever the service is replaced or stopped, so an old
    /// supervisor knows to give up.
    generation: u64,
    pid: Option<i32>,
}

#[derive(Clone, Default)]
pub struct Services(Arc<Mutex<HashMap<String, Service>>>);

impl Services {
    /// Starts spec as the service spec.service, replacing one of that name.
    /// Returns the pid of its first run.
    pub fn start(&self, spec: ExecSpec, reaper: &Reaper) -> anyhow::Result<u32> {
        let name = spec.service.clone().unwrap_or_default();
        self.stop(&name);
        let (child, mut exited) = exec::spawn_detached(&spec, reaper)?;
        let pid = child.id();
        let generation = {
            let mut all = self.0.lock().unwrap();
            let generation = all.get(&name).map_or(0, |s| s.generation) + 1;
            all.insert(name.clone(), Service { generation, pid: Some(pid as i32) });
            generation
        };
        let (services, reaper) = (self.clone(), reaper.clone());
        std::thread::spawn(move || {
            let mut pause = FIRST_PAUSE;
            loop {
                let started = Instant::now();
                let _ = exited.recv();
                if started.elapsed() > HEALTHY {
                    pause = FIRST_PAUSE;
                }
                std::thread::sleep(pause);
                pause = (pause * 2).min(MAX_PAUSE);
                // Hold the table while restarting, so a stop can't slip in
                // between the check and the spawn.
                let mut all = services.0.lock().unwrap();
                match all.get_mut(&name) {
                    Some(s) if s.generation == generation => match exec::spawn_detached(&spec, &reaper) {
                        Ok((child, rx)) => {
                            s.pid = Some(child.id() as i32);
                            exited = rx;
                        }
                        Err(e) => {
                            eprintln!("berth-init: service {name}: {e:#}");
                            s.pid = None;
                            drop(all);
                            // Try again later, with a channel that never fires
                            // until then.
                            let (tx, rx) = std::sync::mpsc::channel();
                            std::thread::spawn(move || {
                                std::thread::sleep(MAX_PAUSE);
                                let _ = tx.send(-1);
                            });
                            exited = rx;
                        }
                    },
                    _ => return,
                }
            }
        });
        Ok(pid)
    }

    /// Stops the service name, if it runs, and keeps it from restarting.
    pub fn stop(&self, name: &str) {
        let pid = {
            let mut all = self.0.lock().unwrap();
            let Some(s) = all.get_mut(name) else { return };
            s.generation += 1;
            s.pid.take()
        };
        let Some(pid) = pid else { return };
        // Each run is its own session and process group.
        unsafe { libc::kill(-pid, libc::SIGTERM) };
        let deadline = Instant::now() + GRACE;
        while Instant::now() < deadline && unsafe { libc::kill(pid, 0) } == 0 {
            std::thread::sleep(Duration::from_millis(50));
        }
        unsafe { libc::kill(-pid, libc::SIGKILL) };
    }
}
