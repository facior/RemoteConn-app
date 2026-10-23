//! Brute-force protection for password attempts.
//!
//! - Per source IP: a few free attempts, then a lockout that doubles with
//!   every further failure (30 s … 1 h).
//! - One attempt in flight per IP: without this, many parallel connections
//!   would all pass the lockout check before the first failure is recorded.
//! - Globally: many failures from many addresses (a distributed guess) slow
//!   every attempt down instead of locking legitimate users out.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::IpAddr;
use std::time::{Duration, Instant};

const FREE_ATTEMPTS: u32 = 3;
const BASE_LOCKOUT: Duration = Duration::from_secs(30);
const MAX_LOCKOUT: Duration = Duration::from_secs(3600);
/// Entries without failures for this long are forgotten.
const FORGET_AFTER: Duration = Duration::from_secs(6 * 3600);
const GLOBAL_WINDOW: Duration = Duration::from_secs(600);
const GLOBAL_FAILURES: usize = 20;
/// Added before answering while the global failure rate is high.
pub const GLOBAL_DELAY: Duration = Duration::from_secs(3);

#[derive(Debug, PartialEq, Eq)]
pub enum Admission {
    Allowed,
    /// Locked out for this long.
    Locked(Duration),
    /// Another attempt from this IP is still running.
    Busy,
}

#[derive(Default)]
pub struct RateLimiter {
    entries: HashMap<IpAddr, Entry>,
    in_flight: HashSet<IpAddr>,
    recent_failures: VecDeque<Instant>,
}

struct Entry {
    failures: u32,
    locked_until: Option<Instant>,
    last_failure: Instant,
}

impl RateLimiter {
    /// Starts an attempt; on `Allowed` the caller must later call
    /// [`RateLimiter::finish`].
    pub fn begin(&mut self, ip: IpAddr, now: Instant) -> Admission {
        if let Some(wait) = self.locked(ip, now) {
            return Admission::Locked(wait);
        }
        if !self.in_flight.insert(ip) {
            return Admission::Busy;
        }
        Admission::Allowed
    }

    /// Ends an attempt started with [`RateLimiter::begin`].
    pub fn finish(&mut self, ip: IpAddr, success: bool, now: Instant) {
        self.in_flight.remove(&ip);
        if success {
            self.entries.remove(&ip);
        } else {
            self.fail(ip, now);
        }
    }

    /// Ends an attempt that never got to check a password (e.g. dropped).
    pub fn abandon(&mut self, ip: IpAddr) {
        self.in_flight.remove(&ip);
    }

    /// True while failures from many addresses suggest a distributed attack.
    pub fn under_attack(&mut self, now: Instant) -> bool {
        while self
            .recent_failures
            .front()
            .is_some_and(|t| now.duration_since(*t) > GLOBAL_WINDOW)
        {
            self.recent_failures.pop_front();
        }
        self.recent_failures.len() >= GLOBAL_FAILURES
    }

    fn locked(&self, ip: IpAddr, now: Instant) -> Option<Duration> {
        let until = self.entries.get(&ip)?.locked_until?;
        until.checked_duration_since(now).filter(|d| !d.is_zero())
    }

    fn fail(&mut self, ip: IpAddr, now: Instant) {
        self.recent_failures.push_back(now);
        if self.entries.len() > 10_000 {
            self.entries
                .retain(|_, e| now.duration_since(e.last_failure) < FORGET_AFTER);
        }
        let entry = self.entries.entry(ip).or_insert(Entry {
            failures: 0,
            locked_until: None,
            last_failure: now,
        });
        entry.failures += 1;
        entry.last_failure = now;
        if entry.failures >= FREE_ATTEMPTS {
            let exp = (entry.failures - FREE_ATTEMPTS).min(10);
            let lockout = (BASE_LOCKOUT * 2u32.pow(exp)).min(MAX_LOCKOUT);
            entry.locked_until = Some(now + lockout);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attempt(rl: &mut RateLimiter, ip: IpAddr, ok: bool, now: Instant) -> Admission {
        let a = rl.begin(ip, now);
        if a == Admission::Allowed {
            rl.finish(ip, ok, now);
        }
        a
    }

    #[test]
    fn locks_after_free_attempts_and_grows() {
        let ip: IpAddr = "10.0.0.5".parse().unwrap();
        let other: IpAddr = "10.0.0.6".parse().unwrap();
        let t0 = Instant::now();
        let mut rl = RateLimiter::default();
        for _ in 0..3 {
            assert_eq!(attempt(&mut rl, ip, false, t0), Admission::Allowed);
        }
        assert_eq!(rl.begin(ip, t0), Admission::Locked(BASE_LOCKOUT));
        assert_eq!(rl.begin(other, t0), Admission::Allowed);
        rl.abandon(other);
        assert_eq!(
            attempt(&mut rl, ip, false, t0 + BASE_LOCKOUT),
            Admission::Allowed
        );
        assert_eq!(
            rl.begin(ip, t0 + BASE_LOCKOUT),
            Admission::Locked(BASE_LOCKOUT * 2)
        );
        assert_eq!(
            attempt(&mut rl, ip, true, t0 + BASE_LOCKOUT * 3),
            Admission::Allowed
        );
        assert_eq!(rl.begin(ip, t0 + BASE_LOCKOUT * 3), Admission::Allowed);
    }

    #[test]
    fn one_attempt_in_flight_per_ip() {
        let ip: IpAddr = "10.0.0.7".parse().unwrap();
        let t0 = Instant::now();
        let mut rl = RateLimiter::default();
        assert_eq!(rl.begin(ip, t0), Admission::Allowed);
        assert_eq!(rl.begin(ip, t0), Admission::Busy);
        rl.finish(ip, false, t0);
        assert_eq!(rl.begin(ip, t0), Admission::Allowed);
    }

    #[test]
    fn distributed_failures_trigger_slowdown() {
        let t0 = Instant::now();
        let mut rl = RateLimiter::default();
        for i in 0..GLOBAL_FAILURES as u8 {
            let ip = IpAddr::from([10, 1, 0, i]);
            attempt(&mut rl, ip, false, t0);
        }
        assert!(rl.under_attack(t0));
        assert!(!rl.under_attack(t0 + GLOBAL_WINDOW + Duration::from_secs(1)));
    }
}
