//! Client-side rate limiting shared across processes.
//!
//! Lexware allows ~2 requests/second per API client (token bucket, all
//! endpoints together). Agents often run several `lxw` processes in
//! parallel, so a per-process limiter is not enough: the schedule lives in a
//! small state file guarded by an exclusive file lock. The algorithm is GCRA
//! (a token bucket expressed as a "theoretical arrival time"): each caller
//! reserves the next free slot under the lock, then sleeps until it.

use crate::config::open_locked;
use std::cell::Cell;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A reserved slot further out than this is treated as a corrupt/stale file.
const MAX_FUTURE_MS: u64 = 120_000;

pub struct RateLimiter {
    path: Option<PathBuf>,
    interval_ms: u64,
    burst: u64,
    local_tat: Cell<u64>,
}

impl RateLimiter {
    /// `requests_per_second <= 0` disables limiting.
    pub fn new(path: Option<PathBuf>, requests_per_second: f64, burst: u32) -> Self {
        let interval_ms = if requests_per_second > 0.0 {
            (1000.0 / requests_per_second).ceil() as u64
        } else {
            0
        };
        RateLimiter {
            path,
            interval_ms,
            burst: burst.max(1) as u64,
            local_tat: Cell::new(0),
        }
    }

    /// Blocks until the next request may be sent; returns how long it waited.
    pub fn acquire(&self) -> Duration {
        if self.interval_ms == 0 {
            return Duration::ZERO;
        }
        let now = now_ms();
        let tolerance = (self.burst - 1) * self.interval_ms;
        let interval = self.interval_ms;
        let start = self.update(|tat| {
            let start = now.max(tat.saturating_sub(tolerance));
            (tat.max(start) + interval, start)
        });
        let wait = Duration::from_millis(start.saturating_sub(now));
        if !wait.is_zero() {
            sleep(wait);
        }
        wait
    }

    /// Pushes the shared schedule back after a 429 so parallel processes back off too.
    pub fn penalize(&self, delay: Duration) {
        if self.interval_ms == 0 {
            return;
        }
        let until = now_ms() + delay.as_millis() as u64;
        self.update(|tat| (tat.max(until), ()));
    }

    fn update<T>(&self, f: impl FnOnce(u64) -> (u64, T)) -> T {
        if let Some(path) = &self.path
            && let Ok(mut file) = open_locked(path)
        {
            let tat = sanitize(read_tat(&mut file));
            let (new_tat, out) = f(tat);
            let _ = write_tat(&mut file, new_tat);
            let _ = file.unlock();
            self.local_tat.set(new_tat);
            return out;
        }
        // No usable state file: fall back to limiting within this process.
        let (new_tat, out) = f(sanitize(self.local_tat.get()));
        self.local_tat.set(new_tat);
        out
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn sanitize(tat: u64) -> u64 {
    if tat > now_ms() + MAX_FUTURE_MS { 0 } else { tat }
}

fn read_tat(file: &mut File) -> u64 {
    let mut s = String::new();
    if file.seek(SeekFrom::Start(0)).is_err() || file.read_to_string(&mut s).is_err() {
        return 0;
    }
    s.trim().parse().unwrap_or(0)
}

fn write_tat(file: &mut File, tat: u64) -> std::io::Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(tat.to_string().as_bytes())?;
    file.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn spaces_requests_across_limiters_sharing_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rl");
        // Two limiters on the same file behave like two processes.
        let a = RateLimiter::new(Some(path.clone()), 20.0, 1); // 50 ms interval
        let b = RateLimiter::new(Some(path), 20.0, 1);
        let start = Instant::now();
        for _ in 0..3 {
            a.acquire();
            b.acquire();
        }
        // 6 requests -> 5 intervals of 50 ms at minimum.
        assert!(start.elapsed() >= Duration::from_millis(240), "{:?}", start.elapsed());
    }

    #[test]
    fn burst_allows_immediate_requests() {
        let dir = tempfile::tempdir().unwrap();
        let rl = RateLimiter::new(Some(dir.path().join("rl")), 1.0, 3);
        let start = Instant::now();
        for _ in 0..3 {
            rl.acquire();
        }
        assert!(start.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn penalize_delays_next_request() {
        let dir = tempfile::tempdir().unwrap();
        let rl = RateLimiter::new(Some(dir.path().join("rl")), 100.0, 1);
        rl.penalize(Duration::from_millis(150));
        assert!(rl.acquire() >= Duration::from_millis(100));
    }

    #[test]
    fn disabled_and_fallback_modes() {
        assert_eq!(RateLimiter::new(None, 0.0, 1).acquire(), Duration::ZERO);
        let rl = RateLimiter::new(None, 20.0, 1);
        rl.acquire();
        assert!(rl.acquire() >= Duration::from_millis(30));
    }
}
