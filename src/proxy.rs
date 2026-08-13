use crate::error::ApiError;
use crate::server::AppState;
use crate::metrics::{HealthSnapshot, ProviderMetricsSnapshot};
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::Value;
use std::time::{Duration, Instant};

/// Proxy a request through the provider chain. Walks the chain until one succeeds.
/// Streams the response back — does NOT buffer full responses (O(1) memory per request).
pub async fn proxy_request(
    State(state): State<AppState>,
    method: Method,
    path: String,
    headers: HeaderMap,
    body: bytes::Bytes,
) -> Result<Response, (StatusCode, Json<Value>)> {
    // Extract the requested model from the body (for routing decisions)
    let requested_model = extract_model(&body);

    // Determine which providers can serve this model, in chain order
    let provider_names: Vec<String> = {
        let config = &state.config;
        config.chain.order.clone()
    };

    let max_retries = state.config.rate_limit.max_retries;
    let initial_backoff = state.config.rate_limit.initial_backoff_ms;

    let mut last_error: Option<ApiError> = None;

    for provider_name in &provider_names {
        let provider = match state.providers.get(provider_name) {
            Some(p) => p,
            None => continue,
        };

        // Skip if provider doesn't serve this model
        if let Some(ref model) = requested_model {
            if !provider.serves_model(model) {
                tracing::debug!(
                    provider = %provider_name,
                    model = %model,
                    "skipping provider — model not served"
                );
                continue;
            }
        }

        // Skip if circuit breaker is open
        if !provider.is_available().await {
            tracing::debug!(provider = %provider_name, "skipping — breaker open");
            continue;
        }

        // Skip if no keys available (unless it's Ollama which needs no keys)
        if provider.key_chain.is_empty() && provider.name != "ollama" {
            tracing::debug!(provider = %provider_name, "skipping — no keys");
            continue;
        }

        // Try this provider, with retries for retriable errors
        let mut attempt = 0;
        loop {
            let start = Instant::now();

            // Clone body bytes for each attempt
            let body_clone = body.clone();
            let content_type = headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/json")
                .to_string();

            match provider
                .proxy_request(method.clone(), &path, body_clone, &content_type)
                .await
            {
                Ok(upstream_response) => {
                    let latency = start.elapsed();
                    provider.record_success(latency).await;

                    tracing::info!(
                        provider = %provider_name,
                        latency_ms = latency.as_millis(),
                        "request succeeded"
                    );

                    // Stream the response back
                    return Ok(stream_response(upstream_response).await);
                }
                Err(err) => {
                    let should_retry = err.should_retry() && attempt < max_retries;
                    provider.record_failure(&err).await;

                    tracing::warn!(
                        provider = %provider_name,
                        attempt = attempt,
                        error = %err,
                        should_retry = should_retry,
                        "request failed"
                    );

                    if should_retry {
                        // Backoff: for rate limits, respect Retry-After
                        let backoff_ms = match &err {
                            ApiError::RateLimited { provider: _, retry_after_secs: Some(secs) } => {
                                secs * 1000
                            }
                            _ => initial_backoff * (2u64.pow(attempt)),
                        };
                        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                        attempt += 1;
                        continue;
                    }

                    last_error = Some(err);
                    break; // Move to next provider
                }
            }
        }
    }

    // All providers exhausted
    let err = last_error.unwrap_or(ApiError::AllProvidersExhausted);
    tracing::error!(error = %err, "all providers exhausted");

    let (status, body) = match &err {
        ApiError::Auth { provider, status, body } => {
            let msg = serde_json::json!({
                "error": {
                    "type": "auth_error",
                    "provider": provider,
                    "status": status,
                    "detail": body,
                }
            });
            (StatusCode::from_u16(*status).unwrap_or(StatusCode::UNAUTHORIZED), msg)
        }
        ApiError::RateLimited { provider, retry_after_secs } => {
            let msg = serde_json::json!({
                "error": {
                    "type": "rate_limited",
                    "provider": provider,
                    "retry_after_secs": retry_after_secs,
                }
            });
            (StatusCode::TOO_MANY_REQUESTS, msg)
        }
        ApiError::Timeout { provider, secs } => {
            let msg = serde_json::json!({
                "error": {
                    "type": "timeout",
                    "provider": provider,
                    "timeout_secs": secs,
                }
            });
            (StatusCode::GATEWAY_TIMEOUT, msg)
        }
        ApiError::ServerError { provider, status, body } => {
            let msg = serde_json::json!({
                "error": {
                    "type": "server_error",
                    "provider": provider,
                    "status": status,
                    "detail": body,
                }
            });
            (StatusCode::BAD_GATEWAY, msg)
        }
        ApiError::NetworkError { provider, msg } => {
            let msg = serde_json::json!({
                "error": {
                    "type": "network_error",
                    "provider": provider,
                    "detail": msg,
                }
            });
            (StatusCode::BAD_GATEWAY, msg)
        }
        ApiError::EmptyResponse { provider } => {
            let msg = serde_json::json!({
                "error": {
                    "type": "empty_response",
                    "provider": provider,
                }
            });
            (StatusCode::BAD_GATEWAY, msg)
        }
        ApiError::AllProvidersExhausted => {
            let msg = serde_json::json!({
                "error": {
                    "type": "all_providers_exhausted",
                    "message": "no provider could fulfill this request",
                }
            });
            (StatusCode::SERVICE_UNAVAILABLE, msg)
        }
    };

    Ok((status, Json(body)).into_response())
}

/// Extract model name from request body for routing.
fn extract_model(body: &[u8]) -> Option<String> {
    if body.is_empty() {
        return None;
    }
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("model")?.as_str().map(|s| s.to_string()))
}

/// Convert an upstream reqwest::Response into a streaming axum::Response.
/// Does NOT buffer the body — streams it through.
async fn stream_response(upstream: reqwest::Response) -> Response {
    let status = StatusCode::from_u16(upstream.status().as_u16())
        .unwrap_or(StatusCode::OK);

    let mut headers = HeaderMap::new();

    // Pass through content-type and other relevant headers
    for (key, value) in upstream.headers().iter() {
        let name = key.as_str();
        if matches!(
            name,
            "content-type" | "content-length" | "transfer-encoding" |
            "cache-control" | "x-request-id" | "openai-organization" |
            "openai-processing-ms" | "openai-version"
        ) {
            if let (Ok(hn), Ok(hv)) = (
                HeaderName::try_from(name),
                HeaderValue::try_from(value.as_bytes()),
            ) {
                headers.insert(hn, hv);
            }
        }
    }

    // Stream the body
    let stream = upstream.bytes_stream();
    let body = Body::from_stream(stream);

    let mut response = Response::builder().status(status);
    response.headers_mut().map(|h| h.extend(headers));

    response.body(body).unwrap_or_else(|_| {
        Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from("stream construction failed"))
            .unwrap()
    })
}

/// Handler for /v1/chat/completions
pub async fn chat_completions(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, (StatusCode, Json<Value>)> {
    proxy_request(State(state), method, "/chat/completions".into(), headers, body).await
}

/// Handler for /v1/embeddings
pub async fn embeddings(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, (StatusCode, Json<Value>)> {
    proxy_request(State(state), method, "/embeddings".into(), headers, body).await
}

/// Handler for /v1/audio/speech
pub async fn audio_speech(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, (StatusCode, Json<Value>)> {
    proxy_request(State(state), method, "/audio/speech".into(), headers, body).await
}

/// Generic proxy for any other path.
pub async fn generic_proxy(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    path: axum::extract::Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, (StatusCode, Json<Value>)> {
    let proxy_path = format!("/{}", path.0);
    proxy_request(State(state), method, proxy_path, headers, body).await
}

/// Health endpoint: GET /health
pub async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let mut snapshots = Vec::new();

    for name in &state.config.chain.order {
        if let Some(provider) = state.providers.get(name) {
            let breaker_state = provider.breaker.state().await;
            let consecutive_failures = provider.breaker.consecutive_failures().await;
            let metrics = provider.metrics.lock().await;

            snapshots.push(HealthSnapshot {
                provider: name.clone(),
                breaker_state: breaker_state.as_str().to_string(),
                consecutive_failures,
                metrics: ProviderMetricsSnapshot::from_metrics(&metrics),
                models: provider.models.clone(),
            });
        }
    }

    let total_requests: u64 = snapshots
        .iter()
        .map(|s| s.metrics.total_requests)
        .sum();

    let total_errors: u64 = snapshots
        .iter()
        .map(|s| s.metrics.failed_requests)
        .sum();

    Json(serde_json::json!({
        "status": "ok",
        "providers": snapshots,
        "summary": {
            "total_requests": total_requests,
            "total_errors": total_errors,
            "chain_order": state.config.chain.order,
        }
    }))
}
