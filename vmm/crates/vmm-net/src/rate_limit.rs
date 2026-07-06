//! Token-bucket rate limiter — used on both net (tap) and block (IOPS/BW).
//!
//! A token-bucket rate limiter on IOPS/bandwidth per device,
//! configured per microVM.
//!
//! The token-bucket math (burst, refill, starvation) is unit-testable
//! without KVM.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenBucket {
    /// Capacity (max tokens).
    pub capacity: u64,
    /// Tokens added per second.
    pub refill_per_sec: u64,
    /// Current tokens.
    pub tokens: u64,
    /// Last refill timestamp (nanos since some epoch).
    pub last_refill_ns: u64,
}

impl TokenBucket {
    pub fn new(capacity: u64, refill_per_sec: u64) -> Self {
        Self {
            capacity,
            refill_per_sec,
            tokens: capacity,
            last_refill_ns: 0,
        }
    }

    /// Refill based on elapsed time. `now_ns` is a monotonically-increasing
    /// clock (caller passes it in so tests can control time).
    pub fn refill(&mut self, now_ns: u64) {
        if self.refill_per_sec == 0 {
            return;
        }
        if now_ns <= self.last_refill_ns {
            return;
        }
        let elapsed_ns = now_ns - self.last_refill_ns;
        let added = (elapsed_ns as u128 * self.refill_per_sec as u128 / 1_000_000_000) as u64;
        self.tokens = self.tokens.saturating_add(added).min(self.capacity);
        self.last_refill_ns = now_ns;
    }

    /// Try to consume `n` tokens. Returns true if allowed.
    pub fn consume(&mut self, n: u64, now_ns: u64) -> bool {
        self.refill(now_ns);
        if self.tokens >= n {
            self.tokens -= n;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_bucket_consumes() {
        let mut b = TokenBucket::new(100, 100);
        assert!(b.consume(50, 0));
        assert_eq!(b.tokens, 50);
    }

    #[test]
    fn empty_bucket_refills_over_time() {
        let mut b = TokenBucket::new(100, 100);
        b.tokens = 0;
        b.last_refill_ns = 0;
        // 1 second later → 100 tokens refilled.
        assert!(b.consume(50, 1_000_000_000));
    }

    #[test]
    fn starvation_when_refill_zero() {
        let mut b = TokenBucket::new(100, 0);
        b.tokens = 0;
        assert!(!b.consume(1, 1_000_000_000));
    }

    #[test]
    fn capped_at_capacity() {
        let mut b = TokenBucket::new(100, 1_000_000);
        b.tokens = 90;
        b.last_refill_ns = 0;
        // Way more than enough time → should cap at capacity.
        b.consume(0, 1_000_000_000);
        assert_eq!(b.tokens, 100);
    }
}
