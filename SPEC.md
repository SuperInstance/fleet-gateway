# Fleet Gateway — Spec

## 1. Architecture

Single Cloudflare Worker (`fleet-gateway`) that serves static HTML for 13 domains from one deployment.

```
Client → CF Edge → fleet-gateway Worker → R2 Bucket → Cache API → Response
```

Components:
- **Worker**: TypeScript, runs on Cloudflare edge. Routes requests by `Host` header.
- **R2 bucket**: `fleet-gateway-assets` — holds `dist/<domain-id>/index.html` per domain.
- **Cache API**: 24h edge cache with stale-while-revalidate.

## 2. Routing Table

| Domain | R2 Key |
|---|---|
| `superinstance.ai` | `dist/superinstance/index.html` |
| `activelog.ai` | `dist/activelog/index.html` |
| `activeledger.ai` | `dist/activeledger/index.html` |
| `studylog.ai` | `dist/studylog/index.html` |
| `personallog.ai` | `dist/personallog/index.html` |
| `dmlog.ai` | `dist/dmlog/index.html` |
| `fishinglog.ai` | `dist/fishinglog/index.html` |
| `playerlog.ai` | `dist/playerlog/index.html` |
| `luciddreamer.ai` | `dist/luciddreamer/index.html` |
| `deckboss.net` | `dist/deckboss/index.html` |
| `purplepincher.org` | `dist/purplepincher/index.html` |
| `lucineer.com` | `dist/lucineer/index.html` |
| `cocapn.com` | `dist/cocapn/index.html` |

All map to `text/html; charset=utf-8`. Static assets (CSS/JS/images) can be added under `dist/<domain-id>/assets/*` later.

## 3. Request Flow

1. Request arrives at edge.
2. Worker extracts `Host` header (strip port, lowercase).
3. Look up host in routing table → get R2 key.
4. Check Cache API for cached response.
5. If cache miss: fetch from R2, write to cache (24h TTL), return response.
6. If cache hit: return cached response; optionally revalidate in background.

## 4. Cache Strategy

- **TTL**: 86,400 seconds (24h).
- **Cache key**: `https://fleet-gateway.cache/<host>/index.html`.
- **stale-while-revalidate**: Serve stale while fetching fresh in background.
- **Cache purge**: redeploy or re-upload to R2 and purge via API if needed.
- **No cache**: for non-GET or unknown hosts.

## 5. Custom Domains

To add a new domain:

1. **DNS**: In Cloudflare dashboard (or external registrar with CF nameservers), add a CNAME record pointing the domain to `fleet-gateway.<subdomain>.workers.dev`.
2. **Custom Domain in Workers**: In the Cloudflare dashboard → Workers & Pages → fleet-gateway → Settings → Domains & Routes → Add Custom Domain. Enter the domain. CF handles SSL automatically.
3. **R2**: Upload `dist/<domain-id>/index.html` to the `fleet-gateway-assets` bucket.
4. **Worker code**: Add the domain → R2 key mapping to the `ROUTES` table in `src/index.ts`.
5. **Deploy**: `npx wrangler deploy`.

## 6. Deployment

```bash
# Create R2 bucket (one-time)
npx wrangler r2 bucket create fleet-gateway-assets

# Upload content
npx wrangler r2 object put fleet-gateway-assets/dist/superinstance/index.html --file ./dist/superinstance/index.html

# Deploy worker
npx wrangler deploy
```

## 7. wrangler.toml

See `wrangler.toml` in repo root.

## 8. Worker Code

See `src/index.ts`.
