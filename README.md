# Fleet Gateway — Multi-Domain CDN Edge Router and API Proxy

`fleet-gateway` is the unified entry point for the Pelagic/Cocapn fleet — a multi-domain web infrastructure serving 13+ domains from a single Cloudflare Worker with R2-backed assets and edge cache acceleration. It routes HTTP requests to per-domain static assets stored in R2, with 24-hour edge caching and automatic content-type detection. Additionally, a Python-based API gateway provides reverse-proxy routing to backend services with rate limiting and health checks.

## Why It Matters

Running 13+ separate web properties traditionally means either:

1. **13 separate deployments** — 13× the operational overhead, 13× the SSL certs, 13× the CI pipelines
2. **A monolithic server** — single point of failure, no edge caching, manual TLS

`fleet-gateway` takes a third approach: **one Cloudflare Worker, one R2 bucket, infinite domains**. Adding a new domain is a one-line config change:

```typescript
"newdomain.com": "dist/newdomain/index.html",
```

Benefits:
- **Edge latency** — served from 300+ Cloudflare PoPs, <50ms TTFB worldwide
- **Zero cold starts** — Workers have no container initialization
- **R2 economics** — $0 egress fees (unlike S3), $0.015/GB stored
- **Unified cache** — one edge cache layer for all domains
- **Co-located API** — Python gateway for dynamic backends alongside static assets

## How It Works

### Static Asset Routing (Cloudflare Worker)

The Worker implements a domain-based router:

```
Request → extract hostname → lookup in ROUTES → fetch from R2 → cache at edge → respond
```

**Domain routing table:**

| Domain | R2 Path |
|---|---|
| superinstance.ai | `dist/superinstance/index.html` |
| activelog.ai | `dist/activelog/index.html` |
| activeledger.ai | `dist/activeledger/index.html` |
| dmlog.ai | `dist/dmlog/index.html` |
| fishinglog.ai | `dist/fishinglog/index.html` |
| lucineer.com | `dist/lucineer/index.html` |
| cocapn.com | `dist/cocapn/index.html` |
| deckboss.net | `dist/deckboss/index.html` |
| ... | ... |

**Path resolution:**
- `/` → `dist/{domain}/index.html`
- `/style.css` → `dist/{domain}/style.css`
- `/api/data` → `dist/{domain}/api/data` (static file, or 404)

### Edge Caching

Every successful R2 fetch is cached at the Cloudflare edge:

```typescript
const cache = caches.default;
const cached = await cache.match(cacheKey(host, path));
if (cached) return cached;  // cache hit — instant

// Cache miss → fetch from R2 → populate cache
response = fetchFromR2(objectKey);
await Promise.race([
  cache.put(cacheKey, response.clone()),
  timeout(5000)  // don't block response on cache write
]);
return response;
```

**Cache TTL:** 86,400 seconds (24 hours), with `stale-while-revalidate=300` for 5-minute stale serving during revalidation.

**Cache key namespace:** `https://fleet-gateway.cache/{host}{path}` — ensures per-domain isolation.

### API Gateway (Python)

For dynamic backends, a Python `HTTPServer`-based gateway proxies requests:

```
Client → /{service}/path → service lookup → proxy → backend → response
```

**Services:**

| Service | Port | Health Check |
|---|---|---|
| plato | 8847 | `/rooms` |
| mud | 4042 | `/` |
| arena | 4044 | `/` |
| terminal | 4060 | `/` |
| dashboard | 4046 | `/` |

**Rate limiting:** Sliding window — 100 requests per 60 seconds per `X-Agent-ID`. Implemented as:

$$\text{allow}(a) = \begin{cases} \text{true} & \text{if } |W_a| < R_{\max} \\ \text{false} & \text{otherwise} \end{cases}$$

Where $W_a$ is the sliding window of request timestamps for agent $a$, and $R_{\max} = 100$.

### Complexity

| Operation | Time | Notes |
|---|---|---|
| Route lookup | O(1) | HashMap (Worker) / dict (Python) |
| R2 object fetch | O(1) | Keyed object store |
| Edge cache match | O(1) | Cloudflare CDN cache |
| Content-type detection | O(1) | Extension lookup table |
| Rate limit check | O(W) | W = window size (≤100 entries) |
| Health check (all services) | O(s) | s = number of services, parallelizable |

## Quick Start

### Cloudflare Worker (Static Assets)

```bash
npm install
npx wrangler deploy
```

```toml
# wrangler.toml
name = "fleet-gateway"
main = "src/index.ts"

[[r2_buckets]]
binding = "ASSETS"
bucket_name = "fleet-assets"
```

### Python Gateway (API Proxy)

```bash
pip install -e .
fleet-gateway  # starts on port 8000
```

### Adding a Domain

1. Upload assets to R2 under `dist/newdomain/`
2. Add route to the `ROUTES` table in `src/index.ts`
3. Deploy: `npx wrangler deploy`

## API

### Worker Endpoints

| Route | Method | Description |
|---|---|---|
| `GET / *` | GET, HEAD | Serve static asset for the requesting hostname |
| Other methods | * | 405 Method Not Allowed |

### Python Gateway Endpoints

| Route | Method | Description |
|---|---|---|
| `/` | GET | API docs and service list |
| `/health` | GET | Health check for all backend services |
| `/{service}/*` | GET, POST | Proxy to backend service |
| `OPTIONS *` | * | CORS preflight |

### Health Check Response

```json
{
  "gateway": "UP",
  "services": {
    "plato": { "status": "UP", "code": 200 },
    "mud": { "status": "UP", "code": 200 },
    "arena": { "status": "DOWN", "code": null }
  }
}
```

## Architecture Notes

`fleet-gateway` implements **γ + η = C**:

- **γ (gamma)**: The routing specification — the domain→R2-path mapping table and the proxy→service registry. This is the *infrastructure declaration*.
- **η (eta)**: The runtime implementations — the Cloudflare Worker with R2 binding and edge cache, the Python HTTPServer with urllib proxying. These are the *delivery mechanisms*.
- **C (Configuration)**: **A unified web presence** — the emergent property when routing rules (γ) are correctly enforced by the edge runtime (η). When aligned, 13+ domains serve correctly from a single deployment, with instant edge cache hits, automatic content-type headers, and coherent rate limiting.

The dual-implementation (TypeScript Worker + Python gateway) reflects a deliberate split: **static assets at the edge** (Worker, R2, global CDN) and **dynamic API routing** (Python, proxy, rate limiting). This mirrors the classical CDN + origin pattern, but with both components in the same repo for unified deployment.

### Cache Write Timeout

The `Promise.race` between `cache.put()` and a 5-second timeout is critical: if Cloudflare's cache write is slow (network congestion, edge node under load), the response is still delivered immediately. The cache write happens asynchronously — if it times out, the next request will re-fetch from R2 and retry the cache write.

## References

- **Cloudflare. (2024).** "Cloudflare Workers Documentation." developers.cloudflare.com. — Worker runtime, R2 bindings, edge cache API.
- **Cloudflare. (2024).** "R2 Object Storage." — Zero-egress S3-compatible storage.
- **Fielding, R. T. (2000).** *Architectural Styles and the Design of Network-Based Software Architectures.* Ph.D. thesis, University of California, Irvine. — REST constraints: uniform interface, layered system, cache.
- **Nygard, M. T. (2018).** *Release It! Design and Deploy Production-Ready Software*, 2nd ed. Pragmatic Bookshelf. — Circuit breakers, rate limiting, and timeout patterns for API gateways.
- **Kleppmann, M. (2017).** *Designing Data-Intensive Applications*, Ch. 1 (Reliability, Scalability, Maintainability). O'Reilly.
- **Richardson, C. (2019).** *Microservices Patterns.* Manning. — API gateway pattern, service discovery, backend-for-frontend.
- **Cloudflare. (2023).** "Cache API for Workers." — Edge cache semantics, `stale-while-revalidate` behavior.

## License

MIT
