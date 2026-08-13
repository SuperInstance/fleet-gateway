# fleet-gateway

Rust API gateway for the fleet: circuit breaker, key chain, provider fallback.

Sits between all API consumers (Python scripts, Rust daemons, TypeScript workers,
shell one-offs) and all API providers (DeepInfra, DeepSeek, Z.ai, local Ollama).

## Quick Start

```bash
# Build
cargo build --release

# Run (uses config/fleet-gateway.toml by default)
RUST_LOG=info ./target/release/fleet-gateway

# Or with explicit config
FLEET_GATEWAY_CONFIG=/path/to/config.toml ./target/release/fleet-gateway
```

## Endpoints

| Method | Path | Description |
|--------|------|-------------|
| POST | `/v1/chat/completions` | OpenAI-compatible chat |
| POST | `/v1/embeddings` | OpenAI-compatible embeddings |
| POST | `/v1/audio/speech` | OpenAI-compatible TTS |
| GET  | `/health` | Provider status + metrics |
| *    | `/v1/{path}` | Generic proxy for any other endpoint |

## How It Works

```
client request → extract model from body → walk provider chain
                                                 ↓
                                    [provider healthy? breaker closed? has keys?]
                                                 ↓
                                    try provider → 200: stream response back
                                                  → 429: backoff + retry
                                                  → 401: mark key bad, next provider
                                                  → 5xx/timeout: next provider
                                                  → fail N times: breaker opens
```

### Circuit Breaker

Each provider has an independent breaker:
- **Closed**: normal operation
- **Open**: after N consecutive failures, reject all requests for cooldown
- **Half-Open**: after cooldown, allow one probe; M successes → Closed

### Key Chain

Multiple API keys per provider. On 401/403, the current key is marked bad and
the next key is used. When all keys are bad, the chain resets (giving them
another chance after cooldown).

### Streaming

Responses are streamed through — the gateway never buffers a full response.
Memory usage is O(1) per request regardless of response size.

## Configuration

See `config/fleet-gateway.toml`. Keys can be overridden via environment:

```bash
# Comma-separated for multiple keys
FLEET_GATEWAY__PROVIDERS__DEEPINFRA__KEYS=key1,key2

# Or standard env patterns
DEEPINFRA_API_KEY=key1
```

## Systemd

```bash
# Install (user-level, no sudo needed)
cp ~/.config/systemd/user/fleet-gateway.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now fleet-gateway

# Check
systemctl --user status fleet-gateway
journalctl --user -u fleet-gateway -f
```

## Testing

```bash
cargo test        # 21 tests
cargo clippy      # zero warnings
```

## Design

From the fleet infrastructure proposals by Claude Opus 5 and KimiCode:
- Memory is O(chunk), never O(corpus) or O(duration)
- Never panic — all errors are classified and handled
- Error taxonomy: AuthError (alarm), RateLimited (backoff), EmptyResponse (retry),
  Timeout (fallback), ServerError (fallback), NetworkError (fallback)
- The gateway is the single point of API access for the entire fleet
- Client shims should fail open: if the gateway is down, call vendors directly
