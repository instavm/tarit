//! Small token-bucket rate limiter for device data paths.

use std::sync::Arc;
use std::time::Instant;

const NANOS_PER_SEC: u64 = 1_000_000_000;

pub trait RateLimitClock: Send + Sync {
    fn now_ns(&self) -> u64;
}

#[derive(Debug)]
struct MonotonicClock {
    start: Instant,
}

impl MonotonicClock {
    fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl RateLimitClock for MonotonicClock {
    fn now_ns(&self) -> u64 {
        self.start.elapsed().as_nanos().min(u64::MAX as u128) as u64
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenBucket {
    capacity: u64,
    refill_per_sec: u64,
    tokens: i128,
    last_refill_ns: u64,
}

impl TokenBucket {
    pub fn new(capacity: u64, refill_per_sec: u64) -> Self {
        Self::new_at(capacity, refill_per_sec, 0)
    }

    pub fn new_at(capacity: u64, refill_per_sec: u64, now_ns: u64) -> Self {
        Self {
            capacity,
            refill_per_sec,
            tokens: capacity as i128,
            last_refill_ns: now_ns,
        }
    }

    pub fn refill(&mut self, now_ns: u64) {
        if self.refill_per_sec == 0 || now_ns <= self.last_refill_ns {
            return;
        }

        let elapsed_ns = now_ns - self.last_refill_ns;
        let added = elapsed_ns as u128 * self.refill_per_sec as u128 / NANOS_PER_SEC as u128;
        if added == 0 {
            return;
        }
        let elapsed_for_added =
            (added * NANOS_PER_SEC as u128 / self.refill_per_sec as u128).max(1);
        let added = added.min(i128::MAX as u128) as i128;
        self.tokens = self.tokens.saturating_add(added).min(self.capacity as i128);
        if self.tokens >= self.capacity as i128 {
            self.last_refill_ns = now_ns;
        } else {
            self.last_refill_ns = self
                .last_refill_ns
                .saturating_add(elapsed_for_added.min(elapsed_ns as u128) as u64);
        }
    }

    #[must_use]
    pub fn consume(&mut self, amount: u64, now_ns: u64) -> bool {
        self.refill(now_ns);
        if !self.can_consume(amount) {
            return false;
        }
        self.consume_without_refill(amount);
        true
    }

    pub fn tokens(&self) -> i128 {
        self.tokens
    }

    fn can_consume(&self, amount: u64) -> bool {
        if amount == 0 {
            return true;
        }
        if self.capacity == 0 {
            return false;
        }
        if amount <= self.capacity {
            self.tokens >= amount as i128
        } else {
            self.tokens >= self.capacity as i128
        }
    }

    fn consume_without_refill(&mut self, amount: u64) {
        self.tokens = self.tokens.saturating_sub(amount as i128);
    }
}

#[derive(Clone)]
pub struct RateLimiter {
    ops: TokenBucket,
    bytes: TokenBucket,
    clock: Arc<dyn RateLimitClock>,
}

impl std::fmt::Debug for RateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateLimiter")
            .field("ops", &self.ops)
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}

impl RateLimiter {
    pub fn new(ops_per_sec: u64, bytes_per_sec: u64) -> Self {
        Self::new_with_clock(ops_per_sec, bytes_per_sec, Arc::new(MonotonicClock::new()))
    }

    pub fn new_with_clock(
        ops_per_sec: u64,
        bytes_per_sec: u64,
        clock: Arc<dyn RateLimitClock>,
    ) -> Self {
        let now_ns = clock.now_ns();
        Self {
            ops: TokenBucket::new_at(ops_per_sec, ops_per_sec, now_ns),
            bytes: TokenBucket::new_at(bytes_per_sec, bytes_per_sec, now_ns),
            clock,
        }
    }

    #[must_use]
    pub fn try_charge(&mut self, ops: u64, bytes: u64) -> bool {
        let now_ns = self.clock.now_ns();
        self.try_charge_at(ops, bytes, now_ns)
    }

    #[must_use]
    pub fn try_charge_at(&mut self, ops: u64, bytes: u64, now_ns: u64) -> bool {
        self.ops.refill(now_ns);
        self.bytes.refill(now_ns);

        if !self.ops.can_consume(ops) || !self.bytes.can_consume(bytes) {
            return false;
        }

        self.ops.consume_without_refill(ops);
        self.bytes.consume_without_refill(bytes);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[derive(Debug)]
    struct ManualClock {
        now_ns: AtomicU64,
    }

    impl ManualClock {
        fn new(now_ns: u64) -> Self {
            Self {
                now_ns: AtomicU64::new(now_ns),
            }
        }

        fn advance(&self, delta_ns: u64) {
            self.now_ns.fetch_add(delta_ns, Ordering::Relaxed);
        }
    }

    impl RateLimitClock for ManualClock {
        fn now_ns(&self) -> u64 {
            self.now_ns.load(Ordering::Relaxed)
        }
    }

    #[test]
    fn token_bucket_charges_within_budget() {
        let mut bucket = TokenBucket::new(10, 10);

        assert!(bucket.consume(4, 0));

        assert_eq!(bucket.tokens(), 6);
    }

    #[test]
    fn token_bucket_rejects_until_refill() {
        let mut bucket = TokenBucket::new(10, 10);

        assert!(bucket.consume(10, 0));
        assert!(!bucket.consume(1, 0));
        assert!(!bucket.consume(1, 99_999_999));
        assert!(bucket.consume(1, 100_000_000));

        assert_eq!(bucket.tokens(), 0);
    }

    #[test]
    fn token_bucket_refill_caps_at_capacity() {
        let mut bucket = TokenBucket::new(10, 10);

        assert!(bucket.consume(9, 0));
        assert!(bucket.consume(0, 5 * NANOS_PER_SEC));

        assert_eq!(bucket.tokens(), 10);
    }

    #[test]
    fn rate_limiter_charges_ops_and_bytes_atomically() {
        let clock = Arc::new(ManualClock::new(0));
        let mut limiter = RateLimiter::new_with_clock(2, 100, clock.clone());

        assert!(limiter.try_charge(1, 80));
        assert!(!limiter.try_charge(1, 30));
        assert!(limiter.try_charge(1, 20));
        assert!(!limiter.try_charge(1, 1));
        clock.advance(NANOS_PER_SEC);
        assert!(limiter.try_charge(1, 100));
    }
}
