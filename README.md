# Fleet Gateway

**A Rust API gateway with circuit breaker, key chain rotation, and provider fallback — the single point of API access for the entire fleet.**

> *Every request walks the chain. Every provider gets one chance. Every key earns its keep.*

![Fleet Gateway — the engine room switchboard](assets/hero_001.jpg)

---

## Table of Contents

- [Vision](#vision)
- [Architecture](#architecture)
- [Quick Start](#quick-start)
- [Key Concepts](#key-concepts)
- [API Reference](#api-reference)
- [Configuration](#configuration)
- [Testing](#testing)
- [Deployment](#deployment)
- [Further Reading](#further-reading)
- [Relation to the Fleet](#relation-to-the-fleet)

---

## Vision

Every fleet application — Python scripts, Rust daemons, TypeScript Cloudflare Workers, shell one-liners — needs to call AI APIs. Without a gateway, each client manages its own API keys, handles its own retries, implements its own fallbacks, and breaks in its own special way when a provider goes down.

**Fleet Gateway solves this by being the single door.** Every API call goes through the gateway. The gateway knows which providers serve which models, which keys are healthy, which providers are up, and how to fall back gracefully when things go wrong. Clients speak [OpenAI-compatible](https://platform.openai.com/docs/api-reference) API to the gateway; the gateway handles everything else.

### Why Not Just Call Providers Directly?

| Without a Gateway | With Fleet Gateway |
|---|---|
| Each client manages API keys | Keys live in one config file |
| Each client implements retry logic | Circuit breaker + exponential backoff built in |
| A dead key crashes the client | Key chain rotates to the next key automatically |
| A provider outage takes you offline | Provider chain falls through to the next provider |
| Rate limits cause scattered failures | Centralized 429 handling with Retry-After awareness |
| No visibility into API health | `/health` endpoint shows all provider metrics |

### Design Principles

1. **[Never panic](https://doc.rust-lang.org/book/ch09-01-unrecoverable-errors-with-panic.html)** — all errors are classified and handled. The gateway is crash-proof.
2. **[O(1) memory per request](https://en.wikipedia.org/wiki/Big_O_notation)** — responses are streamed through, never buffered. The gateway can proxy a 10MB LLM response using constant memory.
3. **[Fail open](https://en.wikipedia.org/wiki/Fail-open)** — if the gateway itself is down, client shims should call providers directly. The gateway is an optimization, not a dependency.
4. **[Separation of concerns](https://en.wikipedia.org/wiki/Separation_of_concerns)** — the gateway routes and protects; it does not transform requests.

---

## Architecture

```
                         ┌─────────────────────────────────┐
                         │        Fleet Gateway             │
                         │        (Axum + Tokio)            │
                         │                                 │
   Client Request ──────►│  Extract model from body        │
   POST /v1/chat/...     │        │                        │
                         │        ▼                        │
                         │  Walk provider chain:            │
                         │  [deepinfra] → [deepseek] →      │
                         │  [zai] → [ollama]                │
                         │        │                        │
                         │  For each provider:              │
                         │   ├─ Serves this model?          │
                         │   ├─ Breaker closed?             │
                         │   ├─ Keys available?             │
                         │   ├─ Send request                │
                         │   ├─ 200 → stream response ◄──  │
                         │   ├─ 429 → backoff, retry        │
                         │   ├─ 401 → mark key bad, next   │
                         │   ├─ 5xx → record, next provider │
                         │   └─ timeout → next provider     │
                         │                                 │
   Client ◄──────────────│  Stream response back            │
                         └─────────────────────────────────┘
```

### Module Map

| Module | Lines | Responsibility |
|--------|-------|---------------|
| [`main.rs`](src/main.rs) | 60 | Entry point: load config, build providers, start server |
| [`server.rs`](src/server.rs) | 68 | Axum router setup, [`AppState`](https://docs.rs/axum/latest/axum/struct.State.html) construction |
| [`proxy.rs`](src/proxy.rs) | 341 | Request routing, provider chain walk, response streaming |
| [`provider.rs`](src/provider.rs) | 166 | Single provider abstraction: HTTP client, model matching, metrics |
| [`circuit_breaker.rs`](src/circuit_breaker.rs) | 228 | Per-provider circuit breaker state machine |
| [`key_chain.rs`](src/key_chain.rs) | 113 | API key rotation with bad-key tracking |
| [`error.rs`](src/error.rs) | 64 | Error taxonomy (Auth, RateLimited, Timeout, ServerError, etc.) |
| [`metrics.rs`](src/metrics.rs) | 95 | Per-provider request counters, latency, error rates |
| [`config.rs`](src/config.rs) | 106 | TOML config parsing with environment variable overrides |

### Data Flow

```
1. Client POSTs to /v1/chat/completions with {"model": "deepseek-chat", ...}
2. proxy.rs extracts the model name from the JSON body
3. For each provider in chain order:
   a. Does this provider serve "deepseek-chat"? → skip if not
   b. Is the circuit breaker closed? → skip if open
   c. Are API keys available? → skip if exhausted
   d. Forward the request with the current key
   e. Classify the response:
      - 200 OK → record success, stream body back to client
      - 429 → respect Retry-After header, backoff, retry (up to max_retries)
      - 401/403 → mark key as bad in key chain, advance to next key
      - 5xx → record failure, try next provider
      - timeout → record failure, try next provider
4. If all providers exhausted → return 503 with diagnostic JSON
```

---

## Quick Start

### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) 1.75+ (2021 edition)
- API keys for at least one provider (DeepInfra, DeepSeek, Z.ai, or local [Ollama](https://ollama.ai))

### Build

```bash
git clone https://github.com/SuperInstance/fleet-gateway.git
cd fleet-gateway
cargo build --release
```

### Configure

Edit `config/fleet-gateway.toml` with your API keys, or provide them via environment variables:

```bash
# Option 1: Environment variables (comma-separated for multiple keys)
export FLEET_GATEWAY__PROVIDERS__DEEPINFRA__KEYS=key1,key2
export FLEET_GATEWAY__PROVIDERS__DEEPSEEK__KEYS=sk-your-key
export FLEET_GATEWAY__PROVIDERS__ZAI__KEYS=your-zai-key

# Option 2: Standard env patterns
export DEEPINFRA_API_KEY=key1
export DEEPSEEK_API_KEY=sk-your-key
```

### Run

```bash
# Default config location: config/fleet-gateway.toml
RUST_LOG=info ./target/release/fleet-gateway

# Explicit config path
FLEET_GATEWAY_CONFIG=/path/to/config.toml ./target/release/fleet-gateway
```

### Test It

```bash
# Health check — see all provider statuses
curl http://127.0.0.1:8787/health | jq .

# Chat completion (OpenAI-compatible)
curl http://127.0.0.1:8787/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "deepseek-chat",
    "messages": [{"role": "user", "content": "Hello!"}],
    "stream": true
  }'
```

---

## Key Concepts

### Circuit Breaker

The [circuit breaker pattern](https://martinfowler.com/bliki/CircuitBreaker.html) prevents cascading failures by stopping requests to a sick provider before it drags the whole system down. Each provider has its own independent breaker.

```
CLOSED ──── (N consecutive failures) ────► OPEN
   ▲                                          │
   │                                          │ (cooldown_secs)
   │                                          ▼
CLOSED ◄──── (M successes) ────────── HALF_OPEN
   ▲                                          │
   └──────────────────────────────────────────┘
                (any failure in HALF_OPEN → OPEN)
```

**States:**
- **Closed** — Normal operation. All requests pass through.
- **Open** — Tripped after `failure_threshold` consecutive failures. All requests rejected immediately. No network calls, no waiting.
- **Half-Open** — After `cooldown_secs`, one probe request is allowed. If it succeeds, the breaker needs `success_threshold` consecutive successes to return to Closed. Any failure re-opens immediately.

**Why this matters:** When DeepInfra goes down, the breaker opens within seconds. Subsequent requests skip DeepInfra entirely and go straight to DeepSeek. The fleet never waits for a timeout on a dead provider.

#### Further Reading on Circuit Breakers

- [Martin Fowler: CircuitBreaker](https://martinfowler.com/bliki/CircuitBreaker.html) — the canonical explanation
- [Microsoft Azure: Circuit Breaker pattern](https://learn.microsoft.com/en-us/azure/architecture/patterns/circuit-breaker) — cloud architecture perspective
- [Release It! (book)](https://pragprog.com/titles/mnee2/release-it-second-edition/) by Michael Nygard — production stability patterns
- [Resilience4j](https://resilience4j.readme.io/) — JVM circuit breaker library (same concepts, different language)

### Key Chain

Most providers allow multiple API keys on one account for [rate limit pooling](https://platform.openai.com/docs/guides/rate-limits). The key chain manages this automatically.

```
[key1] → [key2] → [key3]     ← healthy keys
   ↓
  401  → mark key1 bad
         ↓
[key2] → [key3]              ← key1 skipped until reset
   ↓
  401  → mark key2 bad
         ↓
[key3]                       ← only one key left
   ↓
  401  → ALL keys bad → reset chain, try key1 again
```

When all keys are marked bad, the chain resets — giving the keys another chance after a cooldown cycle. This handles cases like temporary quota resets or billing cycles.

### Provider Chain

The provider chain is ordered fallback. The gateway walks this list on every request, skipping providers that can't serve the model or have open breakers:

```
DeepInfra → DeepSeek → Z.ai → Ollama
```

- **[DeepInfra](https://deepinfra.com)** — 179+ models, cheap, fast. Primary for most calls.
- **[DeepSeek](https://www.deepseek.com)** — Direct API, extremely cheap, excellent reasoning models.
- **[Z.ai](https://chat.z.ai)** — GLM models, unlimited on Max plan.
- **[Ollama](https://ollama.ai)** — Local fallback. Serves any model (runs on localhost).

### Error Taxonomy

Errors are classified into a strict taxonomy — each type triggers a different response:

| Error Type | HTTP Status | Retry? | Breaker Failure? | Action |
|---|---|---|---|---|
| `Auth` | 401/403 | No | Yes | Mark key bad, advance chain |
| `RateLimited` | 429 | Yes (respect `Retry-After`) | Yes | Exponential backoff |
| `Timeout` | — | Yes | Yes | Fall through to next provider |
| `ServerError` | 5xx | Yes | Yes | Fall through to next provider |
| `NetworkError` | — | Yes | Yes | Fall through to next provider |
| `EmptyResponse` | — | Yes | No | Retry (transient) |
| `AllProvidersExhausted` | — | No | No | Return 503 to client |

This taxonomy comes from the fleet infrastructure design (see [Further Reading](#further-reading)). The key insight: not all errors are equal. An auth error means the key is dead — retrying with the same key wastes time. A rate limit means the provider is throttling — waiting and retrying is correct. A 5xx means the provider is broken — falling through immediately is best.

### Streaming

The gateway uses [HTTP streaming](https://developer.mozilla.org/en-US/docs/Web/API/Streams_API) throughout. When a client requests `stream: true`, the gateway streams the upstream response byte-by-byte through to the client. It never buffers a full response body.

This means memory usage is **O(1) per request** regardless of response size. A 10MB LLM completion streams through the gateway using the same ~constant memory as a 1KB response.

The gateway uses [`reqwest::Response::bytes_stream()`](https://docs.rs/reqwest/latest/reqwest/struct.Response.html#method.bytes_stream) on the client side and [`axum::body::Body::from_stream()`](https://docs.rs/axum/latest/axum/body/struct.Body.html#method.from_stream) on the server side, connected by a futures stream pipeline.

---

## API Reference

### `POST /v1/chat/completions`

[OpenAI-compatible](https://platform.openai.com/docs/api-reference/chat/create) chat completions endpoint. Supports streaming (`"stream": true`).

```bash
curl http://127.0.0.1:8787/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "deepseek-chat",
    "messages": [{"role": "user", "content": "What is 2+2?"}]
  }'
```

### `POST /v1/embeddings`

[OpenAI-compatible](https://platform.openai.com/docs/api-reference/embeddings) embeddings endpoint.

```bash
curl http://127.0.0.1:8787/v1/embeddings \
  -H "Content-Type: application/json" \
  -d '{
    "model": "BAAI/bge-m3",
    "input": ["hello world", "goodbye world"]
  }'
```

### `POST /v1/audio/speech`

[OpenAI-compatible](https://platform.openai.com/docs/api-reference/audio/createSpeech) TTS endpoint.

### `GET /health`

Returns provider health, breaker states, and aggregate metrics:

```json
{
  "status": "ok",
  "providers": [
    {
      "provider": "deepinfra",
      "breaker_state": "closed",
      "consecutive_failures": 0,
      "metrics": {
        "total_requests": 1542,
        "successful_requests": 1538,
        "failed_requests": 4,
        "error_rate": 0.0026,
        "avg_latency_ms": 340.5
      },
      "models": ["Qwen/Qwen3-Coder-480B", ...]
    }
  ],
  "summary": {
    "total_requests": 3210,
    "total_errors": 12,
    "chain_order": ["deepinfra", "deepseek", "zai", "ollama"]
  }
}
```

### `* /v1/{path}`

Generic proxy for any other OpenAI-compatible endpoint. The gateway routes it through the same provider chain.

---

## Configuration

Configuration is in [`config/fleet-gateway.toml`](config/fleet-gateway.toml):

```toml
[server]
listen = "127.0.0.1:8787"

[circuit_breaker]
failure_threshold = 5      # consecutive failures before opening
cooldown_secs = 60         # seconds before half-open probe
success_threshold = 2      # successes in half-open before closing

[rate_limit]
max_retries = 2            # retries per provider on 429
initial_backoff_ms = 500   # base for exponential backoff

[providers.deepinfra]
base_url = "https://api.deepinfra.com/v1/openai"
keys = ["your-key"]
models = ["Qwen/Qwen3-Coder-480B", "ByteDance/Seed-2.0-pro", ...]

[providers.deepseek]
base_url = "https://api.deepseek.com/v1"
keys = ["sk-your-key"]
models = ["deepseek-chat", "deepseek-reasoner"]

[chain]
order = ["deepinfra", "deepseek", "zai", "ollama"]
```

### Environment Variable Overrides

| Variable | Format | Example |
|---|---|---|
| `FLEET_GATEWAY_CONFIG` | File path | `/etc/fleet/gateway.toml` |
| `FLEET_GATEWAY__PROVIDERS__{NAME}__KEYS` | Comma-separated | `key1,key2,key3` |
| `{NAME}_API_KEY` | Single key | `DEEPINFRA_API_KEY=xxx` |

### Dependencies

| Crate | Version | Purpose |
|---|---|---|
| [axum](https://docs.rs/axum) | 0.8 | Async HTTP server ([Tokio](https://tokio.rs)-based) |
| [reqwest](https://docs.rs/reqwest) | 0.12 | HTTP client with streaming and [rustls](https://docs.rs/rustls) |
| [tokio](https://docs.rs/tokio) | 1 | Async runtime |
| [dashmap](https://docs.rs/dashmap) | 6 | Lock-free concurrent HashMap for providers |
| [serde](https://docs.rs/serde) / [serde_json](https://docs.rs/serde_json) | 1 | JSON serialization |
| [tracing](https://docs.rs/tracing) | 0.1 | Structured logging |
| [tikv-jemallocator](https://docs.rs/tikv-jemallocator) | 0.6 | System allocator for [better memory fragmentation](https://github.com/tikv/jemallocator) |
| [tower-http](https://docs.rs/tower-http) | 0.6 | CORS and tracing middleware |
| [thiserror](https://docs.rs/thiserror) | 2 | Ergonomic error enums |
| [toml](https://docs.rs/toml) | 0.8 | Config file parsing |

---

## Testing

```bash
cargo test        # 21 unit tests across all modules
cargo clippy      # zero warnings (treat warnings as errors in CI)
cargo bench       # (no benchmarks yet — planned)
```

Tests cover:
- Circuit breaker state transitions (Closed → Open → HalfOpen → Closed/Open)
- Key chain rotation and all-bad reset behavior
- Error classification (is_breaker_failure, should_retry)
- Database operations (via [tempfile](https://docs.rs/tempfile) for isolation)
- Content hashing and chunking

All tests use Tokio's async test harness (`#[tokio::test]`).

---

## Deployment

### Systemd (Production)

The gateway runs as a [systemd user service](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html) — no root needed:

```bash
# Create the service file
cat > ~/.config/systemd/user/fleet-gateway.service << 'EOF'
[Unit]
Description=Fleet Gateway — API gateway with circuit breaker
After=network.target

[Service]
Type=simple
WorkingDirectory=%h/projects/fleet-gateway
Environment=RUST_LOG=info
ExecStart=%h/projects/fleet-gateway/target/release/fleet-gateway
Restart=always
RestartSec=5
MemoryMax=512M

[Install]
WantedBy=default.target
EOF

# Enable and start
systemctl --user daemon-reload
systemctl --user enable --now fleet-gateway

# Monitor
journalctl --user -u fleet-gateway -f
```

### Production Checklist

- [ ] Set `RUST_LOG=info` (or `warn` for quieter operation)
- [ ] Configure at least 2 providers for fallback
- [ ] Set `MemoryMax` in systemd to prevent OOM under load
- [ ] Monitor the `/health` endpoint (e.g., with [Uptime Kuma](https://github.com/louislam/uptime-kuma))
- [ ] Rotate API keys periodically — update config and restart
- [ ] Use `journalctl --user -u fleet-gateway` for log aggregation

---

## Further Reading

### For Developers

- [OpenAI API Reference](https://platform.openai.com/docs/api-reference) — the API standard this gateway implements
- [Axum Documentation](https://docs.rs/axum/latest/axum/) — the Rust web framework used
- [Tokio Tutorial](https://tokio.rs/tokio/tutorial) — async Rust fundamentals
- [Reqwest Streaming](https://docs.rs/reqwest/latest/reqwest/struct.Response.html#method.bytes_stream) — how the gateway streams responses
- [The Rust Book: Error Handling](https://doc.rust-lang.org/book/ch09-00-error-handling.html) — why `Result<T, E>` beats exceptions

### For Engineers (Ops/SRE)

- [Circuit Breaker Pattern (Microsoft Azure)](https://learn.microsoft.com/en-us/azure/architecture/patterns/circuit-breaker) — cloud architecture guidance
- [Martin Fowler: Circuit Breaker](https://martinfowler.com/bliki/CircuitBreaker.html) — the original design pattern
- [Release It! 2nd Edition](https://pragprog.com/titles/mnee2/release-it-second-edition/) by Michael Nygard — production stability patterns
- [Google SRE Book: Handling Overload](https://sre.google/sre-book/handling-overload/) — rate limiting, backoff, and load shedding
- [RFC 7231: HTTP Semantics](https://datatracker.ietf.org/doc/html/rfc7231) — status code semantics (especially 429, 503)
- [The Little Book of Semaphores](https://greenteapress.com/wp/semaphores/) — concurrency primitives (relevant to async mutex patterns)

### For Architects

- [API Gateway Pattern](https://microservices.io/patterns/apigateway.html) — microservices.io
- [Backpressure in Distributed Systems](https://lnishan.github.io/2018/06/05/backpressure/) — why streaming matters
- [The Log: What every software engineer should know](https://engineering.linkedin.com/distributed-systems/log-what-every-software-engineer-should-know-about-real-time-datas-unifying) by Jay Kreps — data flow in distributed systems
- [Zero-Copy Networking](https://blog.cloudflare.com/zero-downtime-restarts/) — relevant to the gateway's streaming architecture

### For Students

- [Big O Notation (Wikipedia)](https://en.wikipedia.org/wiki/Big_O_notation) — why O(1) memory matters
- [Exponential Backoff (Wikipedia)](https://en.wikipedia.org/wiki/Exponential_backoff) — the retry strategy used
- [Rate Limiting (Wikipedia)](https://en.wikipedia.org/wiki/Rate_limiting) — why providers throttle and how to handle it
- [HTTP Status Codes (MDN)](https://developer.mozilla.org/en-US/docs/Web/HTTP/Status) — what 429, 503, etc. mean

---

## Relation to the Fleet

Fleet Gateway is infrastructure — it doesn't produce anything, it routes everything. Every fleet component that calls an AI API goes through the gateway:

| Component | How It Uses the Gateway |
|---|---|
| **[fleet-memory](https://github.com/SuperInstance/fleet-memory)** | Calls `/v1/embeddings` for vector indexing |
| **[fleet-jepa-midi](https://github.com/SuperInstance/fleet-jepa-midi)** | Calls `/v1/chat/completions` for LLM bandleader directives |
| **[fleet-radio](https://github.com/SuperInstance/fleet-radio)** | Calls the gateway for TTS and image generation |
| **[OpenClaw](https://github.com/SuperInstance/openclaw)** | Subagents call the gateway for model inference |
| **Client shims** | Python `openai` library pointed at `http://127.0.0.1:8787/v1` |

### Design Provenance

This gateway implements the fleet infrastructure proposal by Claude Opus 5 and KimiCode (August 2026). The core design rules from that proposal:

- **Memory is O(chunk), never O(corpus) or O(duration)** — streaming throughout
- **Never panic** — all errors classified and handled
- **Error taxonomy**: AuthError (alarm), RateLimited (backoff), EmptyResponse (retry), Timeout (fallback), ServerError (fallback), NetworkError (fallback)
- **The gateway is the single point of API access for the entire fleet**
- **Client shims should fail open**: if the gateway is down, call vendors directly

---

## License

MIT — part of the [SuperInstance](https://github.com/SuperInstance) fleet.
