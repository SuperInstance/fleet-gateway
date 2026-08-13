use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[derive(Debug, Default, Serialize)]
pub struct ProviderMetrics {
    pub total_requests: u64,
    pub successful_requests: u64,
    pub failed_requests: u64,
    pub rate_limited_count: u64,
    pub auth_errors: u64,
    pub timeout_count: u64,
    pub total_latency_ms: AtomicU64,
    pub last_request_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_error: Option<String>,
}

impl ProviderMetrics {
    pub fn new() -> Self {
        Self {
            total_latency_ms: AtomicU64::new(0),
            ..Default::default()
        }
    }

    pub fn record_success(&mut self, latency: Duration) {
        self.total_requests += 1;
        self.successful_requests += 1;
        self.total_latency_ms.fetch_add(latency.as_millis() as u64, Ordering::Relaxed);
        self.last_request_at = Some(chrono::Utc::now());
    }

    pub fn record_failure(&mut self, error: &str) {
        self.total_requests += 1;
        self.failed_requests += 1;
        self.last_error = Some(error.to_string());
        self.last_request_at = Some(chrono::Utc::now());
    }

    pub fn avg_latency_ms(&self) -> f64 {
        let total = self.total_latency_ms.load(Ordering::Relaxed);
        if self.successful_requests == 0 {
            0.0
        } else {
            total as f64 / self.successful_requests as f64
        }
    }

    pub fn error_rate(&self) -> f64 {
        if self.total_requests == 0 {
            0.0
        } else {
            self.failed_requests as f64 / self.total_requests as f64
        }
    }
}

/// Note: ProviderMetrics uses interior mutability for the atomic latency counter,
/// but the counters need &mut for incrementing. For the concurrent case we use
/// Arc<tokio::sync::Mutex<ProviderMetrics>> in the provider state.
/// For health reporting we serialize a snapshot.

#[derive(Debug, Serialize)]
pub struct HealthSnapshot {
    pub provider: String,
    pub breaker_state: String,
    pub consecutive_failures: u32,
    pub metrics: ProviderMetricsSnapshot,
    pub models: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ProviderMetricsSnapshot {
    pub total_requests: u64,
    pub successful_requests: u64,
    pub failed_requests: u64,
    pub error_rate: f64,
    pub avg_latency_ms: f64,
    pub last_request_at: Option<String>,
    pub last_error: Option<String>,
}

impl ProviderMetricsSnapshot {
    pub fn from_metrics(m: &ProviderMetrics) -> Self {
        Self {
            total_requests: m.total_requests,
            successful_requests: m.successful_requests,
            failed_requests: m.failed_requests,
            error_rate: m.error_rate(),
            avg_latency_ms: m.avg_latency_ms(),
            last_request_at: m.last_request_at.map(|dt| dt.to_rfc3339()),
            last_error: m.last_error.clone(),
        }
    }
}
