use crate::circuit_breaker::CircuitBreaker;
use crate::config::ProviderConfig;
use crate::error::ApiError;
use crate::key_chain::KeyChain;
use crate::metrics::ProviderMetrics;
use reqwest::Client;
use std::time::Duration;
use tokio::sync::Mutex;

/// A single API provider with its own circuit breaker, key chain, and metrics.
#[derive(Debug)]
pub struct Provider {
    pub name: String,
    pub base_url: String,
    pub models: Vec<String>,
    pub key_chain: KeyChain,
    pub breaker: CircuitBreaker,
    pub metrics: Mutex<ProviderMetrics>,
    pub client: Client,
}

impl Provider {
    pub fn new(name: &str, config: &ProviderConfig, breaker: CircuitBreaker) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(300))
            .pool_max_idle_per_host(8)
            .build()
            .expect("failed to build HTTP client");

        Self {
            name: name.to_string(),
            base_url: config.base_url.clone(),
            models: config.models.clone(),
            key_chain: KeyChain::new(config.keys.clone()),
            breaker,
            metrics: Mutex::new(ProviderMetrics::new()),
            client,
        }
    }

    /// Check if this provider serves the requested model.
    /// For Ollama, all models are accepted (it's the local fallback).
    pub fn serves_model(&self, model: &str) -> bool {
        if self.name == "ollama" {
            return true; // Local fallback serves everything
        }
        self.models.iter().any(|m| m == model)
    }

    /// Check if the circuit breaker allows a request.
    pub async fn is_available(&self) -> bool {
        self.breaker.allow_request().await
    }

    /// Make a proxied request to this provider.
    /// Returns the reqwest ResponseBuilder for streaming or buffering.
    pub async fn proxy_request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: bytes::Bytes,
        content_type: &str,
    ) -> Result<reqwest::Response, ApiError> {
        let url = format!("{}{}", self.base_url, path);

        let key = self.key_chain.current_key().await;

        let mut req = self.client.request(method.clone(), &url);
        if let Some(ref k) = key {
            req = req.bearer_auth(k);
        }
        req = req.header("Content-Type", content_type);
        req = req.body(body);

        let start = std::time::Instant::now();

        let response = req
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    ApiError::Timeout {
                        provider: self.name.clone(),
                        secs: 300,
                    }
                } else {
                    ApiError::NetworkError {
                        provider: self.name.clone(),
                        msg: e.to_string(),
                    }
                }
            })?;

        let status = response.status().as_u16();
        let latency = start.elapsed();

        tracing::debug!(
            provider = %self.name,
            status = status,
            latency_ms = latency.as_millis(),
            "upstream response"
        );

        // Handle rate limiting (429)
        if status == 429 {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());

            return Err(ApiError::RateLimited {
                provider: self.name.clone(),
                retry_after_secs: retry_after,
            });
        }

        // Handle auth errors (401, 403)
        if status == 401 || status == 403 {
            tracing::warn!("auth error on {}, marking key as bad", self.name);
            self.key_chain.mark_current_bad().await;

            let body = response.text().await.unwrap_or_default();
            return Err(ApiError::Auth {
                provider: self.name.clone(),
                status,
                body,
            });
        }

        // Handle server errors (5xx)
        if status >= 500 {
            let body = response.text().await.unwrap_or_default();
            return Err(ApiError::ServerError {
                provider: self.name.clone(),
                status,
                body,
            });
        }

        // Success path
        Ok(response)
    }

    /// Record a successful request in metrics.
    pub async fn record_success(&self, latency: Duration) {
        self.breaker.record_success().await;
        let mut m = self.metrics.lock().await;
        m.record_success(latency);
    }

    /// Record a failed request in metrics.
    pub async fn record_failure(&self, error: &ApiError) {
        if error.is_breaker_failure() {
            self.breaker.record_failure().await;
        }
        let mut m = self.metrics.lock().await;
        match error {
            ApiError::Auth { .. } => m.auth_errors += 1,
            ApiError::RateLimited { .. } => m.rate_limited_count += 1,
            ApiError::Timeout { .. } => m.timeout_count += 1,
            _ => {}
        }
        m.record_failure(&error.to_string());
    }
}
