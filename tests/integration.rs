use fleet_gateway::circuit_breaker::{BreakerState, CircuitBreaker};
use fleet_gateway::config::Config;
use fleet_gateway::error::ApiError;
use fleet_gateway::key_chain::KeyChain;
use fleet_gateway::metrics::{ProviderMetrics, ProviderMetricsSnapshot};

mod config_tests {
    use super::*;

    #[test]
    fn test_config_parses() {
        let raw = r#"
[server]
listen = "127.0.0.1:8787"

[circuit_breaker]
failure_threshold = 5
cooldown_secs = 60
success_threshold = 2

[rate_limit]
max_retries = 2
initial_backoff_ms = 500

[providers.deepinfra]
base_url = "https://api.deepinfra.com/v1/openai"
keys = ["test-key"]
models = ["Qwen/Qwen3-Coder-480B"]

[providers.ollama]
base_url = "http://localhost:11434/v1"
keys = []
models = ["granite3.1-dense:2b"]

[chain]
order = ["deepinfra", "ollama"]
"#;
        let config: Config = toml::from_str(raw).unwrap();
        assert_eq!(config.server.listen, "127.0.0.1:8787");
        assert_eq!(config.providers.len(), 2);
        assert_eq!(config.chain.order, vec!["deepinfra", "ollama"]);
    }
}

mod breaker_tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn full_cycle() {
        let breaker = CircuitBreaker::new(2, 0, 1);
        assert_eq!(breaker.state().await, BreakerState::Closed);

        breaker.record_failure().await;
        breaker.record_failure().await;
        assert_eq!(breaker.state().await, BreakerState::Open);

        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(breaker.allow_request().await);
        assert_eq!(breaker.state().await, BreakerState::HalfOpen);

        breaker.record_success().await;
        assert_eq!(breaker.state().await, BreakerState::Closed);
    }

    #[tokio::test]
    async fn closed_allows_all() {
        let breaker = CircuitBreaker::new(5, 60, 2);
        assert!(breaker.allow_request().await);
        assert!(breaker.allow_request().await);
        assert!(breaker.allow_request().await);
    }

    #[tokio::test]
    async fn open_rejects() {
        let breaker = CircuitBreaker::new(1, 600, 2);
        breaker.record_failure().await;
        assert!(!breaker.allow_request().await);
    }

    #[tokio::test]
    async fn half_open_failure_reopens() {
        let breaker = CircuitBreaker::new(1, 0, 2);
        breaker.record_failure().await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        breaker.allow_request().await;
        breaker.record_failure().await;
        assert_eq!(breaker.state().await, BreakerState::Open);
    }

    #[tokio::test]
    async fn success_resets_failures() {
        let breaker = CircuitBreaker::new(3, 60, 1);
        breaker.record_failure().await;
        breaker.record_failure().await;
        breaker.record_success().await;
        breaker.record_failure().await;
        breaker.record_failure().await;
        assert_eq!(breaker.state().await, BreakerState::Closed);
    }
}

mod key_chain_tests {
    use super::*;

    #[tokio::test]
    async fn rotation_and_reset() {
        let kc = KeyChain::new(vec!["a".into(), "b".into(), "c".into()]);
        assert_eq!(kc.current_key().await.as_deref(), Some("a"));
        kc.mark_current_bad().await;
        assert_eq!(kc.current_key().await.as_deref(), Some("b"));
        kc.reset().await;
        assert_eq!(kc.current_key().await.as_deref(), Some("b"));
    }

    #[tokio::test]
    async fn all_bad_resets() {
        let kc = KeyChain::new(vec!["a".into(), "b".into()]);
        kc.mark_current_bad().await;
        kc.mark_current_bad().await;
        assert!(kc.current_key().await.is_some());
    }

    #[tokio::test]
    async fn empty_chain() {
        let kc = KeyChain::new(vec![]);
        assert!(kc.is_empty());
        assert!(kc.current_key().await.is_none());
    }
}

mod error_tests {
    use super::*;

    #[test]
    fn auth_not_retriable() {
        let err = ApiError::Auth {
            provider: "test".into(),
            status: 401,
            body: "bad key".into(),
        };
        assert!(!err.should_retry());
        assert!(err.is_breaker_failure());
    }

    #[test]
    fn rate_limited_retriable() {
        let err = ApiError::RateLimited {
            provider: "test".into(),
            retry_after_secs: Some(5),
        };
        assert!(err.should_retry());
        assert!(err.is_breaker_failure());
    }

    #[test]
    fn empty_response_retriable_not_breaker() {
        let err = ApiError::EmptyResponse { provider: "test".into() };
        assert!(err.should_retry());
        assert!(!err.is_breaker_failure());
    }

    #[test]
    fn server_error_5xx_retriable() {
        let err = ApiError::ServerError {
            provider: "test".into(),
            status: 502,
            body: "bad gateway".into(),
        };
        assert!(err.should_retry());
        assert!(err.is_breaker_failure());
    }
}

mod metrics_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn record_and_snapshot() {
        let mut m = ProviderMetrics::new();
        m.record_success(Duration::from_millis(100));
        m.record_success(Duration::from_millis(200));
        m.record_failure("timeout");

        assert_eq!(m.total_requests, 3);
        assert_eq!(m.successful_requests, 2);

        let snap = ProviderMetricsSnapshot::from_metrics(&m);
        assert_eq!(snap.total_requests, 3);
        assert!((snap.avg_latency_ms - 150.0).abs() < 0.01);
        assert!((snap.error_rate - (1.0 / 3.0)).abs() < 0.01);
    }
}
