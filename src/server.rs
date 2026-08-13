use crate::config::Config;
use crate::provider::Provider;
use crate::circuit_breaker::CircuitBreaker;
use axum::{
    response::IntoResponse,
    routing::{any, get, post},
    Json, Router,
};
use std::sync::Arc;
use tower_http::{cors::CorsLayer, trace::TraceLayer};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub providers: Arc<dashmap::DashMap<String, Arc<Provider>>>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        let providers = dashmap::DashMap::new();
        let cb_config = &config.circuit_breaker;

        for (name, provider_config) in &config.providers {
            let breaker = CircuitBreaker::new(
                cb_config.failure_threshold,
                cb_config.cooldown_secs,
                cb_config.success_threshold,
            );
            let provider = Provider::new(name, provider_config, breaker);
            providers.insert(name.clone(), Arc::new(provider));
        }

        Self {
            config: Arc::new(config),
            providers: Arc::new(providers),
        }
    }
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        // OpenAI-compatible endpoints
        .route("/v1/chat/completions", post(crate::proxy::chat_completions))
        .route("/v1/embeddings", post(crate::proxy::embeddings))
        .route("/v1/audio/speech", post(crate::proxy::audio_speech))
        // Generic proxy for any other /v1/* path
        .route("/v1/{*path}", any(crate::proxy::generic_proxy))
        // Health & metrics
        .route("/health", get(crate::proxy::health))
        // Root
        .route("/", get(root_handler))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn root_handler() -> impl IntoResponse {
    Json(serde_json::json!({
        "service": "fleet-gateway",
        "version": env!("CARGO_PKG_VERSION"),
        "endpoints": [
            "POST /v1/chat/completions",
            "POST /v1/embeddings",
            "POST /v1/audio/speech",
            "GET  /health",
        ],
    }))
}
