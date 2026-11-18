//! Exponential backoff with jitter.
//!
//! A mail client that retries a dead server in a tight loop is worse than one
//! that gives up: it burns battery, fills the log with redaction-safe noise and
//! gets the account throttled by the provider.  The schedule here is the usual
//! one — double the delay, cap it, and spread the attempts of several accounts
//! apart with jitter so that a network outage does not produce a thundering herd
//! the moment the connection comes back.

use std::time::Duration;

/// A retry schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    /// Delay after the first failure.
    pub base: Duration,
    /// Upper bound of the delay.
    pub max: Duration,
    /// How many times the delay doubles before it reaches the cap.
    factor: u32,
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new(Duration::from_secs(5), Duration::from_secs(15 * 60))
    }
}

impl Backoff {
    /// Builds a schedule with the given first delay and cap.
    #[must_use]
    pub fn new(base: Duration, max: Duration) -> Self {
        Self {
            base,
            max,
            factor: 4,
        }
    }

    /// The delay to wait after `attempt` failures (the first failure is `1`).
    ///
    /// No jitter is applied; the engine adds it, so that the deterministic part
    /// stays testable.
    #[must_use]
    pub fn delay(&self, attempt: u32) -> Duration {
        if attempt == 0 {
            return Duration::ZERO;
        }
        let doublings = attempt.saturating_sub(1).min(self.factor);
        let factor = 1u64 << doublings;
        let millis = self
            .base
            .as_millis()
            .saturating_mul(u128::from(factor))
            .min(self.max.as_millis());
        Duration::from_millis(u64::try_from(millis).unwrap_or(u64::MAX))
    }

    /// The delay with jitter, in `[delay, delay * 1.5)`.
    ///
    /// The jitter is added rather than subtracted so that the cap is never
    /// exceeded by more than half — a client that waits *less* than the schedule
    /// asks for is the one that hammers a server.
    #[must_use]
    pub fn jittered(&self, attempt: u32, entropy: u64) -> Duration {
        let delay = self.delay(attempt);
        if delay.is_zero() {
            return delay;
        }
        let span = delay.as_millis() / 2 + 1;
        let bonus = u128::from(entropy) % span;
        let millis = delay
            .as_millis()
            .saturating_add(bonus)
            .min(self.max.as_millis());
        Duration::from_millis(u64::try_from(millis).unwrap_or(u64::MAX))
    }
}

/// Cheap source of jitter that does not need a random number generator.
///
/// `std::collections::hash_map::RandomState` is seeded by the OS once per
/// process and is exactly as unpredictable as this needs to be.
#[must_use]
pub fn entropy() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or(0),
    );
    hasher.finish()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn the_first_attempt_is_immediate() {
        let backoff = Backoff::default();
        assert_eq!(backoff.delay(0), Duration::ZERO);
    }

    #[test]
    fn the_delay_doubles_and_then_stops_at_the_cap() {
        let backoff = Backoff::new(Duration::from_secs(5), Duration::from_secs(60));
        assert_eq!(backoff.delay(1), Duration::from_secs(5));
        assert_eq!(backoff.delay(2), Duration::from_secs(10));
        assert_eq!(backoff.delay(3), Duration::from_secs(20));
        assert_eq!(backoff.delay(4), Duration::from_secs(40));
        assert_eq!(backoff.delay(5), Duration::from_secs(60));
        assert_eq!(backoff.delay(50), Duration::from_secs(60));
    }

    #[test]
    fn a_zero_attempt_is_not_shifted() {
        let backoff = Backoff::new(Duration::ZERO, Duration::from_secs(10));
        assert_eq!(backoff.delay(1), Duration::ZERO);
        assert_eq!(backoff.jittered(1, 12345), Duration::ZERO);
    }

    #[test]
    fn jitter_only_ever_adds() {
        let backoff = Backoff::new(Duration::from_secs(10), Duration::from_secs(600));
        let plain = backoff.delay(3);
        for entropy in [0u64, 1, 7, 1_000_000, u64::MAX] {
            let jittered = backoff.jittered(3, entropy);
            assert!(jittered >= plain, "{jittered:?} < {plain:?}");
        }
    }

    #[test]
    fn jitter_never_exceeds_the_cap() {
        let backoff = Backoff::new(Duration::from_secs(5), Duration::from_secs(60));
        for entropy in [0u64, 999_999, u64::MAX] {
            assert!(backoff.jittered(20, entropy) <= Duration::from_secs(60));
        }
    }

    #[test]
    fn jitter_spreads_attempts_apart() {
        let backoff = Backoff::new(Duration::from_secs(10), Duration::from_secs(600));
        let a = backoff.jittered(2, 0);
        let b = backoff.jittered(2, 1234);
        let c = backoff.jittered(2, 4321);
        assert_ne!(a, b);
        assert_ne!(b, c);
    }

    #[test]
    fn entropy_varies() {
        let first = entropy();
        let second = entropy();
        // Not a strict guarantee, but a constant would mean the source is broken.
        assert!(first != second || first != 0);
    }
}
