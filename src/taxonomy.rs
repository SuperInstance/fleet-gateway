//! Error taxonomy for the Fleet Gateway.
//!
//! Client-facing classification of upstream provider failures:
//!
//! | Variant        | Trigger                  | Policy                          |
//! |----------------|--------------------------|---------------------------------|
//! | `AuthError`    | 401/403 (dead key)       | do NOT retry, alarm immediately |
//! | `RateLimited`  | 429 (+ Retry-After)      | backoff, respect Retry-After    |
//! | `EmptyResponse`| 200 with empty body      | retry up to 2x, then advance the chain |
//! | `Timeout`      | request timed out        | retry, then advance the chain   |
//!
//! `classify()` maps an HTTP status code (+ optional Retry-After) to the
//! right variant; `From<reqwest::Error>` maps transport-level failures.

use std::fmt;

/// Maximum retries for `EmptyResponse` before advancing the provider chain.
pub const EMPTY_RESPONSE_MAX_RETRIES: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaxonomyError {
    /// 401/403 — dead or revoked key. No retry; alarm immediately.
    AuthError { status: u16, body: String },
    /// 429 — rate limited. Back off (respect Retry-After when present).
    RateLimited { retry_after_secs: Option<u64> },
    /// 200 with an empty/unusable body. Retry up to
    /// [`EMPTY_RESPONSE_MAX_RETRIES`] times, then advance the chain.
    EmptyResponse,
    /// Request timed out. Retry, then advance the chain.
    Timeout { secs: u64 },
    /// Anything not in the taxonomy (other 4xx/5xx, network, unknown).
    Other { status: Option<u16>, msg: String },
}

impl TaxonomyError {
    /// Whether the caller should retry this error before advancing the
    /// provider chain. `AuthError` never retries.
    pub fn should_retry(&self) -> bool {
        !matches!(self, TaxonomyError::AuthError { .. })
    }

    /// Whether this error warrants an immediate alarm (dead key = human fix).
    pub fn should_alarm(&self) -> bool {
        matches!(self, TaxonomyError::AuthError { .. })
    }

    /// Maximum retry attempts for this error class before chain advancement.
    /// `EmptyResponse` gets its dedicated budget; others retry once.
    pub fn max_retries(&self) -> u32 {
        match self {
            TaxonomyError::EmptyResponse => EMPTY_RESPONSE_MAX_RETRIES,
            TaxonomyError::AuthError { .. } => 0,
            _ => 1,
        }
    }

    /// Map an HTTP status code (and optional Retry-After header value) to
    /// the taxonomy variant.
    pub fn classify(status: u16, retry_after_secs: Option<u64>, body: String) -> Self {
        match status {
            401 | 403 => TaxonomyError::AuthError { status, body },
            429 => TaxonomyError::RateLimited { retry_after_secs },
            408 => TaxonomyError::Timeout { secs: retry_after_secs.unwrap_or(0) },
            200 if body.trim().is_empty() => TaxonomyError::EmptyResponse,
            _ => TaxonomyError::Other {
                status: Some(status),
                msg: body,
            },
        }
    }
}

impl fmt::Display for TaxonomyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TaxonomyError::AuthError { status, .. } => {
                write!(f, "auth error ({}) — dead key, do not retry, alarm", status)
            }
            TaxonomyError::RateLimited { retry_after_secs } => write!(
                f,
                "rate limited — back off (retry after {:?}s)",
                retry_after_secs
            ),
            TaxonomyError::EmptyResponse => write!(
                f,
                "empty response — retry up to {}x then advance chain",
                EMPTY_RESPONSE_MAX_RETRIES
            ),
            TaxonomyError::Timeout { secs } => {
                write!(f, "timeout after {}s — retry then advance chain", secs)
            }
            TaxonomyError::Other { status, msg } => {
                write!(f, "unclassified error (status {:?}): {}", status, msg)
            }
        }
    }
}

impl std::error::Error for TaxonomyError {}

/// Transport-level reqwest failures map to Timeout / Other.
impl From<reqwest::Error> for TaxonomyError {
    fn from(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            TaxonomyError::Timeout { secs: 0 }
        } else {
            TaxonomyError::Other {
                status: e.status().map(|s| s.as_u16()),
                msg: e.to_string(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_auth() {
        let e = TaxonomyError::classify(401, None, "bad key".into());
        assert_eq!(e, TaxonomyError::AuthError { status: 401, body: "bad key".into() });
        assert!(!e.should_retry());
        assert!(e.should_alarm());
        assert_eq!(e.max_retries(), 0);
    }

    #[test]
    fn classifies_rate_limited() {
        let e = TaxonomyError::classify(429, Some(7), "slow down".into());
        assert_eq!(e, TaxonomyError::RateLimited { retry_after_secs: Some(7) });
        assert!(e.should_retry());
        assert!(!e.should_alarm());
    }

    #[test]
    fn classifies_empty_response() {
        let e = TaxonomyError::classify(200, None, "   ".into());
        assert_eq!(e, TaxonomyError::EmptyResponse);
        assert_eq!(e.max_retries(), EMPTY_RESPONSE_MAX_RETRIES);
    }

    #[test]
    fn classifies_timeout_status() {
        let e = TaxonomyError::classify(408, Some(30), String::new());
        assert_eq!(e, TaxonomyError::Timeout { secs: 30 });
        assert!(e.should_retry());
    }

    #[test]
    fn unclassified_passthrough() {
        let e = TaxonomyError::classify(500, None, "boom".into());
        assert!(matches!(e, TaxonomyError::Other { .. }));
        assert!(e.should_retry());
    }
}
