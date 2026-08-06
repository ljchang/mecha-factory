//! In-process rate limiting, because there is nothing in front of this.
//!
//! §13.2 settled on no CDN to begin with: a proxy that terminates TLS reads the
//! plaintext of every request and response, which is the one thing "control our
//! own infrastructure" is actually about. The cost of that decision is honest —
//! no DDoS absorption — and this is what is left: a token bucket per address,
//! in memory, sized so an ordinary reader never meets it.
//!
//! It is deliberately not a defence against a determined flood. It is a defence
//! against the two things that actually happen to a small public box: a script
//! walking the URL space, and a client stuck in a retry loop. For a personal
//! booking page that is the honest scope.
//!
//! **Authenticated requests are not limited here.** Argon2id already makes
//! guessing expensive, and rate-limiting the drain trigger or a publish of a
//! 200 MB notebook would be limiting ourselves. What is limited is everything a
//! stranger can reach.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How many addresses to track before sweeping. A sweep is O(n) over a map that
/// only grows while requests keep arriving, so the bound is on memory rather
/// than on time.
const SWEEP_AT: usize = 10_000;

pub struct RateLimiter {
    per_minute: u32,
    buckets: Mutex<HashMap<IpAddr, Bucket>>,
}

struct Bucket {
    /// Tokens remaining in the current window.
    remaining: u32,
    /// When the window began.
    since: Instant,
}

impl RateLimiter {
    pub fn new(per_minute: u32) -> Self {
        RateLimiter {
            per_minute,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Take one token. `false` means the caller should answer 429.
    pub fn allow(&self, addr: IpAddr) -> bool {
        self.allow_at(addr, Instant::now())
    }

    fn allow_at(&self, addr: IpAddr, now: Instant) -> bool {
        // A limit of zero means "no limiting", not "refuse everything". The
        // other reading turns an unset number into an outage.
        if self.per_minute == 0 {
            return true;
        }
        let mut buckets = self
            .buckets
            .lock()
            .expect("the rate limiter is not poisoned");
        if buckets.len() >= SWEEP_AT {
            buckets.retain(|_, b| now.duration_since(b.since) < Duration::from_secs(60));
        }
        let bucket = buckets.entry(addr).or_insert(Bucket {
            remaining: self.per_minute,
            since: now,
        });
        if now.duration_since(bucket.since) >= Duration::from_secs(60) {
            bucket.remaining = self.per_minute;
            bucket.since = now;
        }
        if bucket.remaining == 0 {
            return false;
        }
        bucket.remaining -= 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(last: u8) -> IpAddr {
        IpAddr::from([10, 0, 0, last])
    }

    #[test]
    fn a_window_holds_and_then_refills() {
        let limiter = RateLimiter::new(3);
        let start = Instant::now();
        for _ in 0..3 {
            assert!(limiter.allow_at(ip(1), start));
        }
        assert!(!limiter.allow_at(ip(1), start), "the fourth is refused");
        // Another address is another bucket, which is the whole point.
        assert!(limiter.allow_at(ip(2), start));

        let later = start + Duration::from_secs(61);
        assert!(limiter.allow_at(ip(1), later), "the window refilled");
    }

    /// An unset limit must read as "do not limit". The other reading turns a
    /// missing number into an outage, which is the wrong direction for a config
    /// mistake on a page people are trying to read.
    #[test]
    fn zero_means_no_limit() {
        let limiter = RateLimiter::new(0);
        let now = Instant::now();
        for _ in 0..1000 {
            assert!(limiter.allow_at(ip(1), now));
        }
    }
}
