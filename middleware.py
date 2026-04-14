"""Gateway Middleware — authentication, rate limiting, CORS, logging, timeout.

Each middleware is a callable that wraps the request handler. Middleware can
short-circuit (return a response directly) or pass through to the next handler.
All middleware uses only the Python standard library.
"""

from __future__ import annotations

import json
import logging
import re
import threading
import time
from collections import defaultdict
from dataclasses import dataclass, field
from typing import Any, Callable, Optional

logger = logging.getLogger("fleet.gateway.middleware")


# ---------------------------------------------------------------------------
# Data helpers
# ---------------------------------------------------------------------------

@dataclass
class Request:
    """Lightweight representation of an incoming HTTP request."""
    method: str
    path: str
    headers: dict[str, str] = field(default_factory=dict)
    body: Optional[bytes] = None
    client_ip: str = "unknown"
    start_time: float = field(default_factory=time.monotonic)

    @property
    def content_type(self) -> str:
        return self.headers.get("content-type", "")


@dataclass
class Response:
    """Lightweight representation of an HTTP response."""
    status: int = 200
    body: bytes | str = b""
    headers: dict[str, str] = field(default_factory=dict)

    def json(self) -> dict[str, Any]:
        if isinstance(self.body, bytes):
            return json.loads(self.body)
        return json.loads(self.body)

    def set_json(self, data: Any, status: int | None = None) -> "Response":
        payload = json.dumps(data).encode()
        self.body = payload
        if status is not None:
            self.status = status
        self.headers["content-type"] = "application/json"
        return self


Handler = Callable[[Request], Response]
Middleware = Callable[[Handler], Handler]


# ---------------------------------------------------------------------------
# Auth Middleware
# ---------------------------------------------------------------------------

class AuthMiddleware:
    """Bearer-token authentication middleware.

    Validates the ``Authorization: Bearer <token>`` header against a
    pre-configured shared fleet secret.  Skips auth for paths in the
    *skip_paths* whitelist.
    """

    def __init__(
        self,
        secret: str = "fleet-secret-token",
        skip_paths: Optional[set[str]] = None,
    ) -> None:
        self.secret = secret
        self.skip_paths: set[str] = skip_paths or {"/health", "/agents", "/registry"}

    def __call__(self, next_handler: Handler) -> Handler:
        def handle(req: Request) -> Response:
            # Allow health / discovery endpoints without auth
            if req.path in self.skip_paths:
                return next_handler(req)

            auth_header = req.headers.get("authorization", "")
            if not auth_header.startswith("Bearer "):
                return Response(status=401).set_json({
                    "error": "missing or invalid authorization header",
                })

            token = auth_header[len("Bearer "):]
            if token != self.secret:
                logger.warning("auth failed for %s from %s", req.path, req.client_ip)
                return Response(status=403).set_json({
                    "error": "invalid bearer token",
                })

            return next_handler(req)
        return handle


# ---------------------------------------------------------------------------
# Rate Limiter  (token-bucket, per client IP)
# ---------------------------------------------------------------------------

class RateLimiter:
    """Token-bucket rate limiter.

    Each client IP gets a bucket that refills at *tokens_per_minute* / 60
    tokens per second.  A burst of up to *burst_size* tokens is allowed.
    """

    def __init__(
        self,
        tokens_per_minute: int = 60,
        burst_size: int = 10,
    ) -> None:
        self.tokens_per_minute = tokens_per_minute
        self.burst_size = burst_size
        self.refill_rate = tokens_per_minute / 60.0

        # {client_ip: {"tokens": float, "last": float}}
        self._buckets: dict[str, dict[str, float]] = defaultdict(
            lambda: {"tokens": float(burst_size), "last": time.monotonic()}
        )
        self._lock = threading.Lock()

    def _consume(self, client_ip: str) -> bool:
        """Try to consume one token. Returns True on success."""
        with self._lock:
            bucket = self._buckets[client_ip]
            now = time.monotonic()
            elapsed = now - bucket["last"]
            bucket["tokens"] = min(
                self.burst_size,
                bucket["tokens"] + elapsed * self.refill_rate,
            )
            bucket["last"] = now

            if bucket["tokens"] >= 1.0:
                bucket["tokens"] -= 1.0
                return True
            return False

    def __call__(self, next_handler: Handler) -> Handler:
        def handle(req: Request) -> Response:
            if not self._consume(req.client_ip):
                logger.info("rate limited %s from %s", req.path, req.client_ip)
                return Response(status=429).set_json({
                    "error": "rate limit exceeded",
                    "retry_after": 60,
                })
            return next_handler(req)
        return handle

    def reset(self) -> None:
        """Clear all buckets (useful for testing)."""
        with self._lock:
            self._buckets.clear()


# ---------------------------------------------------------------------------
# CORS Middleware
# ---------------------------------------------------------------------------

class CORSMiddleware:
    """Add Cross-Origin Resource Sharing headers to responses.

    Optionally handles preflight OPTIONS requests automatically.
    """

    def __init__(
        self,
        allow_origins: str = "*",
        allow_methods: str = "GET, POST, PUT, DELETE, OPTIONS",
        allow_headers: str = "Content-Type, Authorization",
        max_age: int = 86400,
    ) -> None:
        self.allow_origins = allow_origins
        self.allow_methods = allow_methods
        self.allow_headers = allow_headers
        self.max_age = max_age

    def _cors_headers(self) -> dict[str, str]:
        return {
            "Access-Control-Allow-Origin": self.allow_origins,
            "Access-Control-Allow-Methods": self.allow_methods,
            "Access-Control-Allow-Headers": self.allow_headers,
            "Access-Control-Max-Age": str(self.max_age),
        }

    def __call__(self, next_handler: Handler) -> Handler:
        def handle(req: Request) -> Response:
            # Handle preflight
            if req.method == "OPTIONS":
                resp = Response(status=204, body=b"")
                resp.headers.update(self._cors_headers())
                return resp

            resp = next_handler(req)
            resp.headers.update(self._cors_headers())
            return resp
        return handle


# ---------------------------------------------------------------------------
# Logging Middleware
# ---------------------------------------------------------------------------

class LoggingMiddleware:
    """Log every request with method, path, status, and latency."""

    def __init__(self) -> None:
        self.request_log: list[dict[str, Any]] = []
        self._lock = threading.Lock()
        self._counter = 0

    def __call__(self, next_handler: Handler) -> Handler:
        def handle(req: Request) -> Response:
            resp = next_handler(req)
            elapsed_ms = (time.monotonic() - req.start_time) * 1000

            entry = {
                "id": self._next_id(),
                "method": req.method,
                "path": req.path,
                "status": resp.status,
                "latency_ms": round(elapsed_ms, 2),
                "client_ip": req.client_ip,
                "timestamp": time.time(),
            }

            with self._lock:
                self.request_log.append(entry)
                # Keep last 1000 entries
                if len(self.request_log) > 1000:
                    self.request_log = self.request_log[-1000:]

            logger.info(
                "%s %s → %d (%.1fms)",
                req.method, req.path, resp.status, elapsed_ms,
            )
            return resp
        return handle

    def _next_id(self) -> int:
        self._counter += 1
        return self._counter

    def get_log(self) -> list[dict[str, Any]]:
        with self._lock:
            return list(self.request_log)

    def clear_log(self) -> None:
        with self._lock:
            self.request_log.clear()


# ---------------------------------------------------------------------------
# Timeout Middleware
# ---------------------------------------------------------------------------

class TimeoutMiddleware:
    """Per-request timeout wrapper.

    Since we can't easily interrupt a synchronous function in pure stdlib
    without ``signal.alarm`` (which only works in the main thread), this
    middleware is a marker/tracker that other layers can consult.  The actual
    timeout enforcement happens in the gateway handler via
    ``socket.setdefaulttimeout`` / ``urllib.request`` timeout parameter.
    """

    def __init__(self, default_timeout: float = 30.0) -> None:
        self.default_timeout = default_timeout
        # {path_prefix: timeout_seconds}
        self._timeouts: dict[str, float] = {}

    def set_timeout(self, path_prefix: str, timeout: float) -> None:
        """Set a per-path-prefix timeout."""
        self._timeouts[path_prefix] = timeout

    def get_timeout(self, path: str) -> float:
        """Return the timeout configured for *path*, or the default.

        If multiple prefixes match, the longest (most specific) prefix wins.
        """
        best_timeout = self.default_timeout
        best_len = 0
        for prefix, tout in self._timeouts.items():
            if path.startswith(prefix) and len(prefix) > best_len:
                best_timeout = tout
                best_len = len(prefix)
        return best_timeout

    def __call__(self, next_handler: Handler) -> Handler:
        # This middleware is pass-through — the gateway uses get_timeout()
        # when making upstream requests.
        return next_handler


# ---------------------------------------------------------------------------
# Middleware Chain Builder
# ---------------------------------------------------------------------------

class MiddlewareChain:
    """Assemble an ordered chain of middleware around a final handler."""

    def __init__(self) -> None:
        self._middlewares: list[Middleware] = []

    def add(self, middleware: Middleware) -> "MiddlewareChain":
        self._middlewares.append(middleware)
        return self

    def build(self, handler: Handler) -> Handler:
        """Wrap *handler* with all middleware (outermost first)."""
        wrapped = handler
        for mw in reversed(self._middlewares):
            wrapped = mw(wrapped)
        return wrapped
