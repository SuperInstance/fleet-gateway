use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub circuit_breaker: CircuitBreakerConfig,
    pub rate_limit: RateLimitConfig,
    pub providers: HashMap<String, ProviderConfig>,
    pub chain: ChainConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub listen: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CircuitBreakerConfig {
    pub failure_threshold: u32,
    pub cooldown_secs: u64,
    pub success_threshold: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitConfig {
    pub max_retries: u32,
    pub initial_backoff_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    pub base_url: String,
    #[serde(default)]
    pub keys: Vec<String>,
    #[serde(default)]
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChainConfig {
    pub order: Vec<String>,
}

impl Config {
    pub fn load() -> Result<Self, ConfigError> {
        // Try env var for config path, then default locations
        let path = std::env::var("FLEET_GATEWAY_CONFIG").unwrap_or_else(|_| {
            // Look relative to CWD, then config/ dir
            let candidates = [
                "fleet-gateway.toml",
                "config/fleet-gateway.toml",
            ];
            candidates
                .iter()
                .find(|p| std::path::Path::new(p).exists())
                .map(|s| s.to_string())
                .unwrap_or_else(|| "config/fleet-gateway.toml".to_string())
        });

        Self::load_from(&path)
    }

    pub fn load_from(path: &str) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::Read(path.to_string(), e))?;
        let mut config: Config = toml::from_str(&raw)
            .map_err(|e| ConfigError::Parse(path.to_string(), e))?;

        // Override keys from environment if provided (comma-separated)
        for (name, provider) in config.providers.iter_mut() {
            let env_key = format!("FLEET_GATEWAY__PROVIDERS__{}__KEYS", name.to_uppercase());
            if let Ok(val) = std::env::var(&env_key) {
                let keys: Vec<String> = val.split(',').map(|s| s.trim().to_string()).collect();
                if !keys.is_empty() && !keys[0].is_empty() {
                    provider.keys = keys;
                }
            }

            // Also check PROVIDER_API_KEY and PROVIDER_KEY patterns
            let alt_key = format!("{}_API_KEY", name.to_uppercase());
            if let Ok(val) = std::env::var(&alt_key) {
                if !val.is_empty() && provider.keys.is_empty() {
                    provider.keys = vec![val];
                }
            }
        }

        tracing::info!(
            "loaded config from {}: {} providers in chain: {:?}",
            path,
            config.providers.len(),
            config.chain.order
        );

        Ok(config)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config {0}: {1}")]
    Read(String, std::io::Error),
    #[error("failed to parse config {0}: {1}")]
    Parse(String, toml::de::Error),
}
