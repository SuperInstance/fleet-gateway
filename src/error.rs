use serde::Serialize;
use std::fmt;

/// Error classification per the Opus/KimiCode taxonomy:
/// - AuthError (401): do not retry, alarm immediately
/// - RateLimited (429): backoff, respect Retry-After
/// - EmptyResponse (200 empty): retry twice, then fall back
/// - Timeout: retry then fall back
/// - ServerError (5xx): retry then fall back
/// - NetworkError: retry then fall back
#[derive(Debug, Clone, Serialize)]
pub enum ApiError {
    Auth { provider: String, status: u16, body: String },
    RateLimited { provider: String, retry_after_secs: Option<u64> },
    EmptyResponse { provider: String },
    Timeout { provider: String, secs: u64 },
    ServerError { provider: String, status: u16, body: String },
    NetworkError { provider: String, msg: String },
    AllProvidersExhausted,
}

impl ApiError {
    /// Whether this error should count as a circuit-breaker failure.
    pub fn is_breaker_failure(&self) -> bool {
        match self {
            ApiError::Auth { .. } => true,      // dead key
            ApiError::ServerError { .. } => true,
            ApiError::NetworkError { .. } => true,
            ApiError::Timeout { .. } => true,
            ApiError::RateLimited { .. } => true, // persistent 429s mean the provider is struggling
            ApiError::EmptyResponse { .. } => false, // might be transient model issue
            ApiError::AllProvidersExhausted => false,
        }
    }

    /// Whether we should retry on this error before falling through.
    pub fn should_retry(&self) -> bool {
        match self {
            ApiError::Auth { .. } => false,
            ApiError::RateLimited { .. } => true,
            ApiError::EmptyResponse { .. } => true,
            ApiError::Timeout { .. } => true,
            ApiError::ServerError { status, .. } => *status >= 500,
            ApiError::NetworkError { .. } => true,
            ApiError::AllProvidersExhausted => false,
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Auth { provider, status, .. } => write!(f, "auth error on {} ({})", provider, status),
            ApiError::RateLimited { provider, retry_after_secs } => write!(f, "rate limited on {} (retry after {:?}s)", provider, retry_after_secs),
            ApiError::EmptyResponse { provider } => write!(f, "empty response from {}", provider),
            ApiError::Timeout { provider, secs } => write!(f, "timeout on {} ({}s)", provider, secs),
            ApiError::ServerError { provider, status, .. } => write!(f, "server error on {} ({})", provider, status),
            ApiError::NetworkError { provider, msg } => write!(f, "network error on {}: {}", provider, msg),
            ApiError::AllProvidersExhausted => write!(f, "all providers exhausted"),
        }
    }
}

impl std::error::Error for ApiError {}
