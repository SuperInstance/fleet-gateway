# Fleet API Gateway

A single HTTP API that provides a unified interface to the entire Pelagic fleet. Instead of talking to each agent on its own port, you talk to one gateway that routes to the right agent.

## Quick Start

```bash
# Onboard — create a starter fleet.yaml
python cli.py onboard

# Start the gateway (default port 9001)
python cli.py serve

# Or directly
python gateway.py
```

## Endpoints

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/health` | Fleet-wide health aggregation |
| `GET` | `/agents` | List registered agents with status |
| `GET` | `/registry` | Full service registry |
| `POST` | `/registry/register` | Register a new service |
| `DELETE` | `/registry/{name}` | Deregister a service |
| `GET` | `/stats` | Gateway statistics |
| `*` | `/api/{agent}/{path}` | Proxy to fleet agent |

## CLI

```bash
python cli.py serve [--port PORT] [--secret SECRET] [--no-auth] [--no-rate-limit]
python cli.py routes        # list registered routes
python cli.py agents        # list discovered agents
python cli.py test          # connectivity test
python cli.py onboard       # create starter fleet.yaml
```

## Architecture

- **`gateway.py`** — Main gateway server with proxying, health, stats, registry
- **`router.py`** — Path-based request routing with service discovery and load balancing
- **`middleware.py`** — Auth, rate limiting, CORS, logging, timeout middleware
- **`cli.py`** — Command-line interface

## Features

- **Service Discovery** — auto-discover from `fleet.yaml`
- **Request Routing** — `/api/{agent}/{endpoint}` → routes to the right agent
- **Load Balancing** — round-robin across multiple instances
- **Rate Limiting** — token-bucket, per-client (configurable)
- **Authentication** — Bearer token auth (shared fleet secret)
- **Request Logging** — audit log with latency tracking
- **Health Aggregation** — `/health` returns fleet-wide status
- **CORS** — configurable cross-origin headers
- **Timeout** — per-service configurable timeouts
- **Retry** — automatic retry on 5xx errors (1 retry)

## Configuration

### fleet.yaml

```yaml
agents:
  - name: trail-agent
    host: localhost
    port: 8501
    aliases: [trail, encoder]
  - name: git-agent
    host: localhost
    port: 8502
    aliases: [git, repo]
```

### Dynamic Registration

```bash
curl -X POST http://localhost:9001/registry/register \
  -H "Authorization: Bearer fleet-secret-token" \
  -H "Content-Type: application/json" \
  -d '{"name":"my-agent","host":"localhost","port":9000}'
```

## Dependencies

None — stdlib only (`http.server`, `urllib.request`, `json`, `threading`).

## Running Tests

```bash
python -m pytest tests/ -v
# or
python -m unittest tests.test_fleet_gateway -v
```
