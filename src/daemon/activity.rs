//! When berths were last used, and the sweep that idles and stops the ones
//! nobody uses.
//!
//! A berth is in use while a caller has a stream open to it (an exec, a
//! proxied connection), for a while after any request, and while its
//! programs keep a CPU busy (a build, a test run).

use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::Daemon;
use super::berth::State;
use super::cgroup;

/// How often berths are looked at.
const SWEEP: Duration = Duration::from_secs(2);
/// A berth using more of a CPU than this is busy, streams or not.
const BUSY_CPU_SHARE: f64 = 0.03;

#[derive(Default)]
pub struct Activity {
    last: Option<Instant>,
    streams: u32,
    cpu_usec: Option<u64>,
    cpu_at: Option<Instant>,
}

pub type Table = std::sync::Mutex<HashMap<String, Activity>>;

/// Counts a stream to a berth as use for as long as it's held.
pub struct Stream {
    daemon: Arc<Daemon>,
    id: String,
}

impl Drop for Stream {
    fn drop(&mut self) {
        let mut all = self.daemon.activity.lock().unwrap();
        let a = all.entry(self.id.clone()).or_default();
        a.streams = a.streams.saturating_sub(1);
        a.last = Some(Instant::now());
    }
}

impl Daemon {
    /// Notes that the berth was just used.
    pub fn touch(&self, id: &str) {
        self.activity.lock().unwrap().entry(id.to_string()).or_default().last = Some(Instant::now());
    }

    /// How long since the berth was last used; zero while a stream is open.
    pub fn unused_for(&self, id: &str) -> Duration {
        let all = self.activity.lock().unwrap();
        match all.get(id) {
            Some(a) if a.streams > 0 => Duration::ZERO,
            Some(Activity { last: Some(last), .. }) => last.elapsed(),
            _ => Duration::ZERO,
        }
    }

    pub fn forget(&self, id: &str) {
        self.activity.lock().unwrap().remove(id);
        self.locks.lock().unwrap().remove(id);
    }

    /// Counts the berth in use until the returned guard is dropped.
    pub fn stream(self: &Arc<Self>, id: &str) -> Stream {
        let mut all = self.activity.lock().unwrap();
        let a = all.entry(id.to_string()).or_default();
        a.streams += 1;
        a.last = Some(Instant::now());
        Stream { daemon: self.clone(), id: id.to_string() }
    }

    /// Counts the berth in use until the peer of conn hangs up, which is how
    /// long an exec handed to init lasts.
    pub fn stream_until_hangup(self: &Arc<Self>, id: &str, conn: OwnedFd) {
        let guard = self.stream(id);
        std::thread::spawn(move || {
            let mut p = libc::pollfd { fd: conn.as_raw_fd(), events: libc::POLLRDHUP, revents: 0 };
            while unsafe { libc::poll(&mut p, 1, -1) } < 0 {}
            drop(guard);
        });
    }

    /// Runs forever: idles berths unused for their idle_after, and stops idle
    /// ones unused for their stop_after.
    pub fn sweep(self: Arc<Self>) {
        loop {
            std::thread::sleep(SWEEP);
            let berths: Vec<_> = self.berths.lock().unwrap().values().cloned().collect();
            for b in berths {
                let unused_for = {
                    let mut all = self.activity.lock().unwrap();
                    let a = all.entry(b.id.clone()).or_default();
                    let now = Instant::now();
                    if b.state == State::Running
                        && let Some(usage) = cgroup::cpu_usage(&b)
                    {
                        if let (Some(before), Some(at)) = (a.cpu_usec, a.cpu_at) {
                            let share = usage.saturating_sub(before) as f64 / at.elapsed().as_micros().max(1) as f64;
                            if share > BUSY_CPU_SHARE {
                                a.last = Some(now);
                            }
                        }
                        (a.cpu_usec, a.cpu_at) = (Some(usage), Some(now));
                    }
                    let last = *a.last.get_or_insert(now);
                    if a.streams > 0 { Duration::ZERO } else { now - last }
                };
                // Checked again under the berth's lock: a caller may have
                // just come back.
                let result = match b.state {
                    State::Running if b.idle_after > 0 && unused_for.as_secs() >= b.idle_after => {
                        self.idle(&b.id, Some(b.idle_after))
                    }
                    State::Idle if b.stop_after > 0 && unused_for.as_secs() >= b.stop_after => {
                        self.stop(&b.id, Some(b.stop_after))
                    }
                    _ => continue,
                };
                if let Err(e) = result {
                    eprintln!("berthd: berth {}: {e:#}", b.id);
                }
            }
        }
    }
}
