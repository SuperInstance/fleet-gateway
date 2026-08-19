//! Phase 3 acceptance: bounded memory on the streaming proxy path.
//!
//! The gateway's core memory promise (`O(chunk)`, never `O(body)`) lives in
//! `proxy::stream_response`, which converts an upstream `reqwest::Response`
//! into an axum `Response` via `Body::from_stream(upstream.bytes_stream())`
//! — no buffering of the full body.
//!
//! Two layers are pinned here:
//!
//! 1. **Network-free** (`chunked_reader_does_not_buffer_whole_body`): a unit
//!    test of the exact streaming primitive the proxy uses. A lazily
//!    producing chunk source is fed through `Body::from_stream` and consumed
//!    frame-by-frame; we assert the first frame arrives long before the
//!    source finishes (no whole-body buffering) and that at most a couple of
//!    chunks are ever alive at once (O(chunk) memory).
//!
//!    Why not call `stream_response` directly: it is private to the crate and
//!    takes a live `reqwest::Response`, which cannot be fabricated without a
//!    real HTTP round trip. The primitive it delegates to is pinned instead.
//!
//! 2. **Live, `#[ignore]`-gated** (`proxy_streams_upstream_without_buffering`):
//!    end-to-end through the real router and a real (loopback) mock upstream.
//!    Gated because it binds a TCP listener — it is self-contained (spawns its
//!    own mock upstream; does NOT need the gateway on :8787 to be running)
//!    but is excluded from plain `cargo test` so CI never flakes on socket
//!    availability.
//!
//!    Run it with:
//!    ```sh
//!    cargo test --test test_streaming_bounded -- --ignored --nocapture
//!    ```
//!    Or all ignored tests: `cargo test -- --ignored`.

use axum::body::Body;
use bytes::Bytes;
use futures::stream::Stream;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Network-free unit test: the streaming primitive is O(chunk), not O(body).
// ---------------------------------------------------------------------------

/// A chunk source that produces `total` chunks of `chunk_len` bytes
/// *lazily* and counts how many chunks are simultaneously alive, so a
/// buffering reader is detectable.
struct LazyChunks {
    chunk_len: usize,
    total: usize,
    /// Mirror of `produced`, visible to the test thread.
    produced_counter: Arc<AtomicUsize>,
    /// Live chunk count (incremented on produce, decremented by the consumer
    /// after each chunk is fully consumed).
    live: Arc<AtomicUsize>,
    /// High-water mark of `live`.
    max_live: Arc<AtomicUsize>,
}

impl Stream for LazyChunks {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        if this.produced_counter.load(Ordering::SeqCst) >= this.total {
            return Poll::Ready(None);
        }
        let produced = this.produced_counter.fetch_add(1, Ordering::SeqCst) + 1;
        let n = this.live.fetch_add(1, Ordering::SeqCst) + 1;
        // Track the high-water mark of concurrently-live chunks.
        this.max_live.fetch_max(n, Ordering::SeqCst);
        // Each chunk is tagged with its index so integrity is checkable.
        let mut chunk = vec![0u8; this.chunk_len];
        chunk[0..8].copy_from_slice(&(produced as u64).to_be_bytes());
        Poll::Ready(Some(Ok(Bytes::from(chunk))))
    }
}

/// 4096 chunks × 8 KiB = 32 MiB total — big enough that a buffering reader
/// would be glaringly obvious, small enough to stay fast.
const TOTAL_CHUNKS: usize = 4096;
const CHUNK_LEN: usize = 8 * 1024;

#[tokio::test]
async fn chunked_reader_does_not_buffer_whole_body() {
    let live = Arc::new(AtomicUsize::new(0));
    let max_live = Arc::new(AtomicUsize::new(0));
    let produced_counter = Arc::new(AtomicUsize::new(0));

    let source = LazyChunks {
        chunk_len: CHUNK_LEN,
        total: TOTAL_CHUNKS,
        produced_counter: Arc::clone(&produced_counter),
        live: Arc::clone(&live),
        max_live: Arc::clone(&max_live),
    };

    // Exactly what proxy::stream_response builds from the upstream:
    // Body::from_stream(upstream.bytes_stream()).
    let body = Body::from_stream(source);
    let mut stream = body.into_data_stream();

    let mut received_bytes: usize = 0;
    let mut received_chunks: usize = 0;
    let mut produced_at_first_chunk: Option<usize> = None;

    // Track "produced but not yet consumed" via the live counter directly:
    // the source bumps `live` on produce; we drop it after consuming a chunk.
    let start = Instant::now();

    while let Some(chunk) = futures::StreamExt::next(&mut stream).await {
        let chunk = chunk.expect("stream error");
        if produced_at_first_chunk.is_none() {
            // How far had the source progressed when the FIRST chunk became
            // readable? If the pipeline buffered the whole body, this would
            // be TOTAL_CHUNKS.
            produced_at_first_chunk = Some(produced_counter.load(Ordering::SeqCst));
        }
        // Integrity: each chunk carries its 1-based index in the first 8 bytes.
        let idx = u64::from_be_bytes(chunk[0..8].try_into().unwrap()) as usize;
        assert_eq!(idx, received_chunks + 1, "chunk order/integrity broken");
        assert_eq!(chunk.len(), CHUNK_LEN);

        received_bytes += chunk.len();
        received_chunks += 1;
        // Chunk fully consumed — release it.
        live.fetch_sub(1, Ordering::SeqCst);
    }

    let elapsed = start.elapsed();

    // 1. Everything arrived, in chunk-sized frames (no coalescing).
    assert_eq!(received_chunks, TOTAL_CHUNKS);
    assert_eq!(received_bytes, TOTAL_CHUNKS * CHUNK_LEN);

    // 2. The first chunk was readable while the source had produced only a
    //    handful of chunks — the reader does NOT wait for the whole body.
    let at_first = produced_at_first_chunk.expect("no chunks received");
    assert!(
        at_first <= 8,
        "first chunk only appeared after {at_first} chunks were produced — \
         reader looks like it buffers (expected immediate, total is {TOTAL_CHUNKS})"
    );

    // 3. Bounded memory: at no point were more than a couple of chunks alive,
    //    i.e. memory is O(chunk), never O(body).
    let peak = max_live.load(Ordering::SeqCst);
    assert!(
        peak <= 4,
        "peak of {peak} simultaneously-live chunks — memory grew with the body"
    );

    // Sanity: it completed (fast — this is in-memory work).
    assert!(elapsed < Duration::from_secs(30), "test took {elapsed:?}");
}

// ---------------------------------------------------------------------------
// Live end-to-end streaming test (#[ignore]: binds a loopback listener).
// ---------------------------------------------------------------------------

/// Full path: mock upstream (slow drip, loopback) → real Provider + proxy
/// chain → real router → our client. Proves the gateway does not buffer the
/// upstream body: headers arrive in a small fraction of the total transfer
/// time, and every byte arrives intact.
///
/// This test is `#[ignore]`d because it binds a TCP socket. It is fully
/// self-contained — it spawns its own mock upstream on an ephemeral loopback
/// port and drives the router in-process, so it does NOT require a gateway
/// instance on :8787.
///
/// Run with:
/// ```sh
/// cargo test --test test_streaming_bounded -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "binds a loopback mock-upstream socket; run: cargo test --test test_streaming_bounded -- --ignored"]
async fn proxy_streams_upstream_without_buffering() {
    use axum::routing::post;
    use axum::Router;
    use fleet_gateway::config::{
        ChainConfig, CircuitBreakerConfig, Config, ProviderConfig, RateLimitConfig, ServerConfig,
    };
    use fleet_gateway::server::{build_router, AppState};
    use std::collections::HashMap;
    use tower::ServiceExt; // oneshot

    // --- mock upstream parameters ---
    const UP_CHUNKS: usize = 40;
    const UP_CHUNK_LEN: usize = 64 * 1024; // 64 KiB → 2.5 MiB total
    const UP_DRIP: Duration = Duration::from_millis(20); // 40 × 20ms ≈ 800ms

    // Mock upstream: drips UP_CHUNKS chunks, one per UP_DRIP.
    let mock_app = Router::new().route(
        "/v1/openai/chat/completions",
        post(move || async move {
            let body_stream = futures::stream::unfold(
                0usize,
                move |i| async move {
                    if i >= UP_CHUNKS {
                        None
                    } else {
                        tokio::time::sleep(UP_DRIP).await;
                        let mut chunk = vec![b'x'; UP_CHUNK_LEN];
                        chunk[0..8].copy_from_slice(&((i + 1) as u64).to_be_bytes());
                        Some((Ok::<_, std::io::Error>(Bytes::from(chunk)), i + 1))
                    }
                },
            );
            axum::response::Response::builder()
                .status(200)
                .header("content-type", "application/octet-stream")
                .body(Body::from_stream(body_stream))
                .unwrap()
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock upstream");
    let mock_addr = listener.local_addr().expect("mock addr");
    let mock = tokio::spawn(async move {
        axum::serve(listener, mock_app).await.expect("mock upstream");
    });

    // --- gateway config: one provider pointing at the mock ---
    let config = Config {
        server: ServerConfig { listen: "127.0.0.1:0".into() },
        circuit_breaker: CircuitBreakerConfig {
            failure_threshold: 5,
            cooldown_secs: 60,
            success_threshold: 2,
        },
        rate_limit: RateLimitConfig { max_retries: 0, initial_backoff_ms: 1 },
        providers: {
            let mut m = HashMap::new();
            m.insert(
                "mock".into(),
                ProviderConfig {
                    base_url: format!("http://{}/v1/openai", mock_addr),
                    keys: vec!["test-key".into()],
                    models: vec!["test-model".into()],
                },
            );
            m
        },
        chain: ChainConfig { order: vec!["mock".into()] },
    };

    let router = build_router(AppState::new(config));

    // --- drive the router in-process (no gateway TCP socket needed) ---
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({"model": "test-model", "messages": []}).to_string(),
        ))
        .expect("build request");

    let t0 = Instant::now();
    let response = router
        .oneshot(request)
        .await
        .expect("router call failed");
    let ttfb = t0.elapsed(); // headers ready — for a streaming proxy this is ~1 drip, not the whole body

    assert_eq!(response.status(), axum::http::StatusCode::OK);

    // --- consume the streamed body ---
    // NOTE: TCP/hyper may split or coalesce frames, so received frames do NOT
    // necessarily align with upstream chunk boundaries. Integrity is checked
    // at chunk boundaries via a rolling byte offset instead of per-frame.
    let mut received_frames = 0usize;
    let mut received_bytes = 0usize;
    let mut first_chunk_at: Option<Duration> = None;
    let mut stream = response.into_body().into_data_stream();
    while let Some(chunk) = futures::StreamExt::next(&mut stream).await {
        let chunk = chunk.expect("body chunk error");
        if first_chunk_at.is_none() {
            first_chunk_at = Some(t0.elapsed());
        }
        // At every upstream chunk boundary, the first 8 bytes carry that
        // chunk's 1-based index. A frame may start mid-chunk (split) or span
        // several chunks (coalesced) — only check when aligned.
        if received_bytes % UP_CHUNK_LEN == 0 && chunk.len() >= 8 {
            let idx = u64::from_be_bytes(chunk[0..8].try_into().unwrap()) as usize;
            let expected = received_bytes / UP_CHUNK_LEN + 1;
            assert_eq!(
                idx, expected,
                "upstream chunk tag corrupted at boundary {expected}"
            );
        }
        received_bytes += chunk.len();
        received_frames += 1;
    }
    let total = t0.elapsed();

    // --- assertions ---
    // All bytes arrived intact, and the body was chunk-aligned end to end.
    assert_eq!(received_bytes, UP_CHUNKS * UP_CHUNK_LEN, "byte count mismatch");
    assert_eq!(received_bytes % UP_CHUNK_LEN, 0, "body not a whole number of chunks");
    assert!(
        received_frames >= 1,
        "body arrived in zero frames — nothing was streamed"
    );

    // STREAMING PROOF: headers (and the first chunk) arrived in a small
    // fraction of the total transfer time. A buffering proxy would hold the
    // response until the upstream finished (~UP_CHUNKS × UP_DRIP ≈ 800ms),
    // making ttfb ≈ total.
    let min_total = UP_DRIP * (UP_CHUNKS as u32); // ~800ms
    assert!(
        total >= min_total,
        "upstream drip finished suspiciously fast ({total:?} < {min_total:?}) — timings invalid"
    );
    assert!(
        ttfb < min_total / 2,
        "time-to-first-byte {ttfb:?} is not meaningfully below total transfer \
         {total:?} — the proxy appears to buffer the whole body before responding"
    );
    let first = first_chunk_at.expect("no body chunks received");
    assert!(
        first < min_total / 2,
        "first body chunk at {first:?} — body appears buffered (total {total:?})"
    );

    mock.abort(); // mock upstream is no longer needed
}
