//! Standalone token-bucket limiter. Active device data paths currently use
//! `vmm-devices::rate_limit`, not this exported scaffold.
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
    /// Fractional token credit, in billionths of a token. Missing remainder
    /// fields in legacy map-based encodings default to zero credit.
    #[serde(default)]
    refill_remainder: u64,
}

impl TokenBucket {
    pub fn new(capacity: u64, refill_per_sec: u64) -> Self {
        Self {
            capacity,
            refill_per_sec,
            tokens: capacity,
            last_refill_ns: 0,
            refill_remainder: 0,
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
        // Keep fractional credit across calls, including unsuccessful consume
        // attempts. Narrow only after capping: a large elapsed/rate product can
        // otherwise wrap the u64 token count before the capacity check.
        let credit =
            elapsed_ns as u128 * self.refill_per_sec as u128 + self.refill_remainder as u128;
        let tokens = self.tokens as u128 + credit / 1_000_000_000;
        self.tokens = tokens.min(self.capacity as u128) as u64;
        self.refill_remainder = if tokens >= self.capacity as u128 {
            0 // A full bucket cannot bank credit beyond its burst capacity.
        } else {
            (credit % 1_000_000_000) as u64
        };
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
    fn frequent_failed_consumes_preserve_fractional_refill() {
        let mut bucket = TokenBucket::new(100, 3);
        assert!(bucket.consume(100, 0));
        let mut consumed = 0;
        for now in 1..=1000 {
            if bucket.consume(1, now * 10_000_000) {
                consumed += 1;
            }
        }
        assert_eq!(consumed, 30, "polling must not erase fractional tokens");
    }

    #[test]
    fn fractional_credit_survives_serialization_and_old_buckets_default_to_zero() {
        let mut bucket = TokenBucket::new(1, 1);
        assert!(bucket.consume(1, 0));
        assert!(!bucket.consume(1, 600_000_000));
        let encoded = serde_json::to_string(&bucket).unwrap();
        let mut restored: TokenBucket = serde_json::from_str(&encoded).unwrap();
        assert!(restored.consume(1, 1_000_000_000));
        let mut legacy: TokenBucket = serde_json::from_str(
            r#"{"capacity":1,"refill_per_sec":1,"tokens":0,"last_refill_ns":0}"#,
        )
        .unwrap();
        assert!(legacy.consume(1, 1_000_000_000));
    }

    #[test]
    fn full_bucket_discards_excess_credit_and_large_refills_do_not_wrap() {
        let mut bucket = TokenBucket::new(1, 1);
        assert!(bucket.consume(1, 0));
        bucket.refill(1_500_000_000);
        assert!(bucket.consume(1, 1_500_000_000));
        assert!(!bucket.consume(1, 2_000_000_000));
        assert!(bucket.consume(1, 2_500_000_000));
        let mut huge = TokenBucket::new(u64::MAX, u64::MAX);
        huge.tokens = 0;
        huge.refill(u64::MAX);
        assert_eq!(huge.tokens, u64::MAX);
        assert_eq!(huge.refill_remainder, 0);
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
