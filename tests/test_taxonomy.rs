//! Phase 3 acceptance: pin the error-taxonomy mapping.
//!
//! `TaxonomyError::classify()` is the contract that drives retry policy,
//! alarming, and chain advancement. These tests pin each documented row of
//! the table in `src/taxonomy.rs`:
//!
//! | Variant         | Trigger             | Policy                            |
//! |-----------------|---------------------|-----------------------------------|
//! | `AuthError`     | 401/403             | do NOT retry, alarm immediately   |
//! | `RateLimited`   | 429 (+ Retry-After) | backoff, respect Retry-After      |
//! | `EmptyResponse` | 200, empty body     | retry 2x, then advance the chain  |
//! | `Timeout`       | 408 / transport t/o | retry, then advance the chain     |
//!
//! Pure mapping tests — no network, no async.

use fleet_gateway::taxonomy::{TaxonomyError, EMPTY_RESPONSE_MAX_RETRIES};

mod classify {
    use super::*;

    // ---------------- AuthError: 401 / 403 ----------------

    #[test]
    fn status_401_maps_to_auth_error() {
        let e = TaxonomyError::classify(401, None, "invalid api key".into());
        assert_eq!(
            e,
            TaxonomyError::AuthError { status: 401, body: "invalid api key".into() }
        );
    }

    #[test]
    fn status_403_maps_to_auth_error() {
        let e = TaxonomyError::classify(403, None, "forbidden".into());
        assert_eq!(
            e,
            TaxonomyError::AuthError { status: 403, body: "forbidden".into() }
        );
    }

    #[test]
    fn auth_error_never_retries_and_alarms() {
        for status in [401u16, 403] {
            let e = TaxonomyError::classify(status, None, String::new());
            assert!(!e.should_retry(), "{status} must not be retried (dead key)");
            assert!(e.should_alarm(), "{status} must alarm immediately (human fix)");
            assert_eq!(e.max_retries(), 0, "{status} retry budget must be zero");
        }
    }

    // ---------------- RateLimited: 429 ----------------

    #[test]
    fn status_429_maps_to_rate_limited_with_retry_after() {
        let e = TaxonomyError::classify(429, Some(7), "slow down".into());
        assert_eq!(e, TaxonomyError::RateLimited { retry_after_secs: Some(7) });
        assert!(e.should_retry(), "429 backs off and retries");
        assert!(!e.should_alarm(), "429 is routine, not alarming");
        assert_eq!(e.max_retries(), 1);
    }

    #[test]
    fn status_429_without_retry_after_is_none() {
        let e = TaxonomyError::classify(429, None, String::new());
        assert_eq!(e, TaxonomyError::RateLimited { retry_after_secs: None });
        // Backoff falls back to the default policy when the header is absent.
        assert!(e.should_retry());
    }

    // ---------------- EmptyResponse: 200 + empty body ----------------

    #[test]
    fn status_200_empty_body_maps_to_empty_response() {
        let e = TaxonomyError::classify(200, None, String::new());
        assert_eq!(e, TaxonomyError::EmptyResponse);
    }

    #[test]
    fn status_200_whitespace_only_body_is_still_empty() {
        // Body is trimmed before the emptiness check — a body of spaces,
        // tabs, and newlines is "empty" for taxonomy purposes.
        let e = TaxonomyError::classify(200, None, "  \n\t  ".into());
        assert_eq!(e, TaxonomyError::EmptyResponse);
    }

    #[test]
    fn empty_response_policy_is_retry_2x_then_advance() {
        let e = TaxonomyError::classify(200, None, String::new());
        assert_eq!(e.max_retries(), EMPTY_RESPONSE_MAX_RETRIES);
        assert_eq!(EMPTY_RESPONSE_MAX_RETRIES, 2, "EmptyResponse budget is pinned at 2");
        assert!(e.should_retry(), "EmptyResponse is retried before chain advance");
        assert!(!e.should_alarm(), "EmptyResponse is not alarming");
    }

    // ---------------- Timeout: 408 ----------------

    #[test]
    fn status_408_maps_to_timeout() {
        let e = TaxonomyError::classify(408, None, String::new());
        assert_eq!(e, TaxonomyError::Timeout { secs: 0 });
    }

    #[test]
    fn status_408_carries_retry_after_as_timeout_secs() {
        // For 408, the Retry-After value is reinterpreted as the timeout
        // duration (see classify()).
        let e = TaxonomyError::classify(408, Some(30), String::new());
        assert_eq!(e, TaxonomyError::Timeout { secs: 30 });
        assert!(e.should_retry(), "timeout is retried, then chain advances");
        assert!(!e.should_alarm());
        assert_eq!(e.max_retries(), 1);
    }

    // ---------------- Everything else: Other ----------------

    #[test]
    fn status_500_maps_to_other() {
        let e = TaxonomyError::classify(500, None, "boom".into());
        assert_eq!(
            e,
            TaxonomyError::Other { status: Some(500), msg: "boom".into() }
        );
        assert!(e.should_retry());
        assert!(!e.should_alarm());
    }

    #[test]
    fn status_200_with_real_body_is_other_not_empty() {
        // Pinned current behavior: a 200 with a non-empty body does not match
        // any taxonomy row and falls through to Other. (A real 200 never
        // reaches classify() in the proxy — the success path streams it —
        // so this is a defensive mapping, pinned here on purpose.)
        let e = TaxonomyError::classify(200, None, "{\"ok\":true}".into());
        assert!(matches!(e, TaxonomyError::Other { status: Some(200), .. }));
    }
}

mod policy_summary {
    use super::*;

    /// Only AuthError alarms — everything else is routine gateway life.
    #[test]
    fn only_auth_errors_alarm() {
        let cases = vec![
            TaxonomyError::classify(401, None, String::new()),
            TaxonomyError::classify(403, None, String::new()),
            TaxonomyError::classify(429, Some(1), String::new()),
            TaxonomyError::classify(429, None, String::new()),
            TaxonomyError::classify(408, None, String::new()),
            TaxonomyError::classify(200, None, String::new()),
            TaxonomyError::classify(500, None, String::new()),
            TaxonomyError::classify(503, None, String::new()),
            TaxonomyError::Other { status: None, msg: "connection reset".into() },
        ];
        for e in cases {
            let is_auth = matches!(e, TaxonomyError::AuthError { .. });
            assert_eq!(
                e.should_alarm(),
                is_auth,
                "should_alarm must be true iff AuthError, got {:?}",
                e
            );
        }
    }

    /// Only AuthError refuses to retry.
    #[test]
    fn only_auth_errors_are_non_retriable() {
        let cases = vec![
            TaxonomyError::classify(429, Some(1), String::new()),
            TaxonomyError::classify(408, Some(5), String::new()),
            TaxonomyError::classify(200, None, " ".into()),
            TaxonomyError::classify(500, None, String::new()),
            TaxonomyError::Other { status: None, msg: String::new() },
        ];
        for e in cases {
            assert!(e.should_retry(), "unexpected non-retriable error: {:?}", e);
        }
    }

    /// Display strings carry the operator-facing policy so logs are
    /// self-explanatory.
    #[test]
    fn display_mentions_policy() {
        let auth = TaxonomyError::classify(401, None, String::new());
        let display = auth.to_string().to_lowercase();
        assert!(display.contains("do not retry"), "auth display: {display}");
        assert!(display.contains("alarm"), "auth display: {display}");

        let rl = TaxonomyError::classify(429, Some(9), String::new());
        assert!(rl.to_string().to_lowercase().contains("back off"));

        let er = TaxonomyError::classify(200, None, String::new());
        assert!(er.to_string().contains("advance chain"));

        let to = TaxonomyError::classify(408, Some(2), String::new());
        assert!(to.to_string().to_lowercase().contains("timeout"));
    }
}
