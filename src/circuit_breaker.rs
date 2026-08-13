use chrono::{DateTime, Utc};
use std::time::Duration;
use tokio::sync::Mutex;

/// Circuit breaker states per the fleet infrastructure design.
/// CLOSED → (N failures) → OPEN → (cooldown) → HALF_OPEN → (M successes) → CLOSED
///                                   ↓ (failure)
///                                 OPEN
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    /// Normal operation. Requests pass through.
    Closed,
    /// Tripped. All requests are rejected immediately.
    /// Entered after `failure_threshold` consecutive failures.
    Open,
    /// Probing. One request is allowed through to test the waters.
    /// Entered after `cooldown_secs` in Open state.
    HalfOpen,
}

impl BreakerState {
    pub fn as_str(&self) -> &'static str {
        match self {
            BreakerState::Closed => "closed",
            BreakerState::Open => "open",
            BreakerState::HalfOpen => "half_open",
        }
    }
}

#[derive(Debug)]
pub struct CircuitBreaker {
    state: Mutex<BreakerState>,
    consecutive_failures: Mutex<u32>,
    consecutive_successes: Mutex<u32>,
    opened_at: Mutex<Option<DateTime<Utc>>>,
    failure_threshold: u32,
    cooldown: Duration,
    success_threshold: u32,
}

impl CircuitBreaker {
    pub fn new(failure_threshold: u32, cooldown_secs: u64, success_threshold: u32) -> Self {
        Self {
            state: Mutex::new(BreakerState::Closed),
            consecutive_failures: Mutex::new(0),
            consecutive_successes: Mutex::new(0),
            opened_at: Mutex::new(None),
            failure_threshold,
            cooldown: Duration::from_secs(cooldown_secs),
            success_threshold,
        }
    }

    /// Check if a request can pass. Returns true if allowed.
    /// If in Open state and cooldown has elapsed, transitions to HalfOpen.
    pub async fn allow_request(&self) -> bool {
        let mut state = self.state.lock().await;

        match *state {
            BreakerState::Closed => true,
            BreakerState::Open => {
                // Check if cooldown has elapsed
                let opened_at = self.opened_at.lock().await;
                if let Some(opened) = *opened_at {
                    if Utc::now().signed_duration_since(opened).num_seconds()
                        >= self.cooldown.as_secs() as i64
                    {
                        tracing::info!("circuit breaker transitioning OPEN → HALF_OPEN");
                        *state = BreakerState::HalfOpen;
                        drop(opened_at);
                        return true;
                    }
                }
                false
            }
            BreakerState::HalfOpen => true, // Allow probe requests
        }
    }

    /// Record a successful request. In HalfOpen, accumulates successes
    /// until threshold reached → Closed.
    pub async fn record_success(&self) {
        let mut state = self.state.lock().await;
        let mut failures = self.consecutive_failures.lock().await;
        let mut successes = self.consecutive_successes.lock().await;

        *failures = 0;

        match *state {
            BreakerState::Closed => {
                *successes = 0;
            }
            BreakerState::HalfOpen => {
                *successes += 1;
                if *successes >= self.success_threshold {
                    tracing::info!("circuit breaker HALF_OPEN → CLOSED");
                    *state = BreakerState::Closed;
                    *successes = 0;
                    let mut opened = self.opened_at.lock().await;
                    *opened = None;
                }
            }
            BreakerState::Open => {
                // Shouldn't happen (request shouldn't have been allowed)
                *state = BreakerState::HalfOpen;
                *successes = 1;
            }
        }
    }

    /// Record a failed request. In Closed, accumulates failures
    /// until threshold reached → Open. In HalfOpen, immediately → Open.
    pub async fn record_failure(&self) {
        let mut state = self.state.lock().await;
        let mut failures = self.consecutive_failures.lock().await;
        let mut successes = self.consecutive_successes.lock().await;
        let mut opened_at = self.opened_at.lock().await;

        *successes = 0;
        *failures += 1;

        match *state {
            BreakerState::Closed => {
                if *failures >= self.failure_threshold {
                    tracing::warn!(
                        "circuit breaker CLOSED → OPEN ({} consecutive failures)",
                        *failures
                    );
                    *state = BreakerState::Open;
                    *opened_at = Some(Utc::now());
                }
            }
            BreakerState::HalfOpen => {
                tracing::warn!("circuit breaker HALF_OPEN → OPEN (probe failed)");
                *state = BreakerState::Open;
                *opened_at = Some(Utc::now());
            }
            BreakerState::Open => {
                // Already open, just increment failures
            }
        }
    }

    /// Get current state for health reporting.
    pub async fn state(&self) -> BreakerState {
        *self.state.lock().await
    }

    pub async fn consecutive_failures(&self) -> u32 {
        *self.consecutive_failures.lock().await
    }

    /// Reset the breaker to Closed (e.g., after cooldown + key reset).
    pub async fn reset(&self) {
        let mut state = self.state.lock().await;
        let mut failures = self.consecutive_failures.lock().await;
        let mut opened_at = self.opened_at.lock().await;

        *state = BreakerState::Closed;
        *failures = 0;
        *opened_at = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_trips_after_threshold() {
        let breaker = CircuitBreaker::new(3, 60, 2);
        assert_eq!(breaker.state().await, BreakerState::Closed);

        breaker.record_failure().await;
        breaker.record_failure().await;
        assert_eq!(breaker.state().await, BreakerState::Closed);

        breaker.record_failure().await;
        assert_eq!(breaker.state().await, BreakerState::Open);

        // Should reject requests now
        assert!(!breaker.allow_request().await);
    }

    #[tokio::test]
    async fn test_half_open_to_closed() {
        let breaker = CircuitBreaker::new(1, 0, 2);

        breaker.record_failure().await;
        assert_eq!(breaker.state().await, BreakerState::Open);

        // Cooldown is 0, so next allow_request should transition to HalfOpen
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        assert!(breaker.allow_request().await);
        assert_eq!(breaker.state().await, BreakerState::HalfOpen);

        breaker.record_success().await;
        assert_eq!(breaker.state().await, BreakerState::HalfOpen);
        breaker.record_success().await;
        assert_eq!(breaker.state().await, BreakerState::Closed);
    }

    #[tokio::test]
    async fn test_half_open_failure_reopens() {
        let breaker = CircuitBreaker::new(1, 0, 2);

        breaker.record_failure().await;
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        breaker.allow_request().await; // → HalfOpen
        breaker.record_failure().await; // → Open again

        assert_eq!(breaker.state().await, BreakerState::Open);
    }

    #[tokio::test]
    async fn test_success_resets_failures() {
        let breaker = CircuitBreaker::new(3, 60, 1);
        breaker.record_failure().await;
        breaker.record_failure().await;
        breaker.record_success().await;
        breaker.record_failure().await;
        breaker.record_failure().await;

        // 2 consecutive failures after reset — should still be closed
        assert_eq!(breaker.state().await, BreakerState::Closed);
    }
}
