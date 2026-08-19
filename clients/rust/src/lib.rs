//! fleet_gw_client — fail-open client shim for the Fleet Gateway.
//!
//! Contract: `post(provider, path, payload)` tries the gateway first
//! (2s connect timeout). If the gateway is unreachable (connection error
//! or timeout), we fall through to the direct vendor API using keys from
//! the environment (ZAI_API_KEY / DEEPSEEK_API_KEY / DEEPINFRA_API_KEY).
//! The gateway routes by model via its provider chain, so the `provider`
//! argument is only used for direct fallback routing.

use reqwest::blocking::Client;
use std::time::Duration;

pub const DEFAULT_GATEWAY_URL: &str = "http://127.0.0.1:8787";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Generous total timeout — chat completions can stream for a while.
/// Only the *connect* phase gets the fast 2s fail-open cutoff.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(300);

/// Base URLs for direct vendor fallback (mirrors config/fleet-gateway.toml).
fn provider_base_url(provider: &str) -> Option<&'static str> {
    match provider {
        "zai" => Some("https://api.z.ai/api/paas/v4"),
        "deepseek" => Some("https://api.deepseek.com/v1"),
        "deepinfra" => Some("https://api.deepinfra.com/v1/openai"),
        _ => None,
    }
}

/// Env var holding the vendor API key for each provider.
fn provider_env_key(provider: &str) -> Option<&'static str> {
    match provider {
        "zai" => Some("ZAI_API_KEY"),
        "deepseek" => Some("DEEPSEEK_API_KEY"),
        "deepinfra" => Some("DEEPINFRA_API_KEY"),
        _ => None,
    }
}

#[derive(Debug)]
pub enum ClientError {
    /// Both gateway and direct fallback were attempted; this is the last error.
    AllFailed(String),
    /// Unknown provider for direct fallback and gateway was unreachable.
    NoDirectRoute { provider: String, reason: String },
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::AllFailed(msg) => write!(f, "gateway and direct call failed: {}", msg),
            ClientError::NoDirectRoute { provider, reason } => write!(
                f,
                "gateway unreachable ({}) and no direct route for provider '{}'",
                reason, provider
            ),
        }
    }
}

impl std::error::Error for ClientError {}

pub struct FleetGwClient {
    gateway_url: String,
    http: Client,
}

impl Default for FleetGwClient {
    fn default() -> Self {
        Self::new(DEFAULT_GATEWAY_URL)
    }
}

impl FleetGwClient {
    pub fn new(gateway_url: &str) -> Self {
        let http = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(TOTAL_TIMEOUT)
            .build()
            .expect("failed to build HTTP client");
        Self {
            gateway_url: gateway_url.trim_end_matches('/').to_string(),
            http,
        }
    }

    /// Fail-open POST: gateway first, direct vendor fallback.
    ///
    /// * `provider` — "zai" | "deepseek" | "deepinfra" (used for fallback only)
    /// * `path` — path under the gateway, e.g. "/v1/chat/completions"
    /// * `payload` — JSON body (must carry the `model` field for gateway routing)
    pub fn post(
        &self,
        provider: &str,
        path: &str,
        payload: &serde_json::Value,
    ) -> Result<serde_json::Value, ClientError> {
        // 1) Try the gateway (fast-fail on connect errors / timeouts).
        let gw_url = format!("{}{}", self.gateway_url, path);
        match self.http.post(&gw_url).json(payload).send() {
            Ok(resp) if resp.status().is_success() => match resp.json::<serde_json::Value>() {
                Ok(body) => return Ok(body),
                Err(e) => {
                    // Malformed body from the gateway — fall through to direct.
                    eprintln!("fleet_gw_client: gateway returned unparseable body: {}", e);
                }
            },
            Ok(resp) => {
                // Gateway is up but rejected the request (4xx/5xx).
                // Fail-open semantics: try the direct vendor call too.
                eprintln!(
                    "fleet_gw_client: gateway returned {}, falling through to direct {}",
                    resp.status(),
                    provider
                );
            },
            Err(e) if e.is_connect() || e.is_timeout() => {
                eprintln!(
                    "fleet_gw_client: gateway unreachable ({}), falling through to direct {}",
                    e, provider
                );
            },
            Err(e) => {
                eprintln!("fleet_gw_client: gateway request error: {}, falling through", e);
            },
        }

        // 2) Direct vendor fallback.
        self.post_direct(provider, path, payload)
    }

    /// Direct vendor call using the provider's API key from the environment.
    pub fn post_direct(
        &self,
        provider: &str,
        path: &str,
        payload: &serde_json::Value,
    ) -> Result<serde_json::Value, ClientError> {
        let base = match provider_base_url(provider) {
            Some(b) => b,
            None => {
                return Err(ClientError::NoDirectRoute {
                    provider: provider.to_string(),
                    reason: "gateway unreachable".to_string(),
                })
            }
        };
        let key_env = provider_env_key(provider)
            .unwrap_or_default();
        let key = std::env::var(key_env).ok().filter(|k| !k.is_empty());

        let url = format!("{}{}", base, path);
        let mut req = self.http.post(&url);
        if let Some(k) = &key {
            req = req.bearer_auth(k);
        }

        let resp = req
            .json(payload)
            .send()
            .map_err(|e| ClientError::AllFailed(format!("direct {} call failed: {}", provider, e)))?;

        let status = resp.status();
        resp.json::<serde_json::Value>()
            .map_err(|e| ClientError::AllFailed(format!(
                "direct {} returned {} with unparseable body: {}",
                provider, status, e
            )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_urls_match_known_providers() {
        assert_eq!(provider_base_url("zai"), Some("https://api.z.ai/api/paas/v4"));
        assert_eq!(provider_base_url("deepseek"), Some("https://api.deepseek.com/v1"));
        assert_eq!(
            provider_base_url("deepinfra"),
            Some("https://api.deepinfra.com/v1/openai")
        );
        assert_eq!(provider_base_url("ollama"), None);
    }

    #[test]
    fn unknown_provider_direct_yields_no_route() {
        let client = FleetGwClient::new("http://127.0.0.1:59999"); // nothing listening
        let payload = serde_json::json!({"model": "x", "messages": []});
        let err = client.post("bogus", "/v1/chat/completions", &payload).unwrap_err();
        assert!(matches!(err, ClientError::NoDirectRoute { .. }));
    }
}
