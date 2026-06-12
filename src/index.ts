export interface Env {
  ASSETS: R2Bucket;
}

const ROUTES: Record<string, string> = {
  "superinstance.ai": "dist/superinstance/index.html",
  "activelog.ai": "dist/activelog/index.html",
  "activeledger.ai": "dist/activeledger/index.html",
  "studylog.ai": "dist/studylog/index.html",
  "personallog.ai": "dist/personallog/index.html",
  "dmlog.ai": "dist/dmlog/index.html",
  "fishinglog.ai": "dist/fishinglog/index.html",
  "playerlog.ai": "dist/playerlog/index.html",
  "luciddreamer.ai": "dist/luciddreamer/index.html",
  "deckboss.net": "dist/deckboss/index.html",
  "purplepincher.org": "dist/purplepincher/index.html",
  "lucineer.com": "dist/lucineer/index.html",
  "cocapn.com": "dist/cocapn/index.html",
};

const CACHE_TTL = 86_400; // 24 hours

function resolveHost(host: string): string | null {
  const normalized = host.split(":")[0].toLowerCase();
  return ROUTES[normalized] ?? null;
}

function cacheKey(host: string, path: string): Request {
  return new Request(`https://fleet-gateway.cache/${host}${path}`);
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    // Only handle GET/HEAD
    if (request.method !== "GET" && request.method !== "HEAD") {
      return new Response("Method not allowed", { status: 405 });
    }

    const url = new URL(request.url);
    const host = url.hostname;
    const r2Key = resolveHost(host);

    if (!r2Key) {
      return new Response("Not found", { status: 404 });
    }

    // Static asset paths under the domain prefix
    let objectKey: string;
    if (url.pathname === "/" || url.pathname === "/index.html") {
      objectKey = r2Key;
    } else {
      // Derive domain ID from the R2 key prefix
      const domainId = r2Key.split("/")[1];
      objectKey = `dist/${domainId}${url.pathname}`;
    }

    // Check cache
    const cache = caches.default;
    const cached = await cache.match(cacheKey(host, url.pathname));
    if (cached) {
      return cached;
    }

    // Fetch from R2
    const object = await env.ASSETS.get(objectKey);
    if (!object) {
      return new Response("Not found", { status: 404 });
    }

    // Determine content type
    const ext = objectKey.split(".").pop() ?? "";
    const contentTypes: Record<string, string> = {
      html: "text/html; charset=utf-8",
      css: "text/css; charset=utf-8",
      js: "application/javascript; charset=utf-8",
      json: "application/json",
      png: "image/png",
      jpg: "image/jpeg",
      jpeg: "image/jpeg",
      svg: "image/svg+xml",
      ico: "image/x-icon",
      webp: "image/webp",
      woff2: "font/woff2",
      woff: "font/woff",
    };
    const contentType = contentTypes[ext] ?? "application/octet-stream";

    const headers = new Headers();
    headers.set("Content-Type", contentType);
    headers.set("Cache-Control", `public, max-age=${CACHE_TTL}, stale-while-revalidate=300`);
    headers.set("ETag", object.httpEtag);
    object.writeHttpMetadata(headers);

    const response = new Response(object.body, { headers });

    // Cache at edge (wait up to 5s, skip if slow)
    const cachePut = cache.put(cacheKey(host, url.pathname), response.clone());
    const timeout = new Promise<"timeout">((resolve) => setTimeout(() => resolve("timeout"), 5000));
    await Promise.race([cachePut, timeout]);

    return response;
  },
} satisfies ExportedHandler<Env>;
