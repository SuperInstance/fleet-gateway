"""Request Router — maps incoming paths to fleet-agent backends.

Routes follow the pattern ``/api/{agent_name}/{endpoint}`` and are resolved
to ``http://{host}:{port}/{endpoint}``.  The router supports:

* Friendly-name → host:port mapping
* Regex-based route patterns for advanced matching
* Round-robin load balancing when multiple backends exist
* Middleware chain integration
"""

from __future__ import annotations

import logging
import re
import threading
from dataclasses import dataclass, field
from typing import Any, Optional

from middleware import (
    AuthMiddleware,
    CORSMiddleware,
    Handler,
    LoggingMiddleware,
    MiddlewareChain,
    RateLimiter,
    Request,
    Response,
    TimeoutMiddleware,
)

logger = logging.getLogger("fleet.gateway.router")


# ---------------------------------------------------------------------------
# Service Registry Entry
# ---------------------------------------------------------------------------

@dataclass
class ServiceInstance:
    """A single running instance of a fleet agent."""
    host: str
    port: int
    healthy: bool = True
    last_check: float = 0.0

    @property
    def base_url(self) -> str:
        return f"http://{self.host}:{self.port}"


@dataclass
class ServiceRegistration:
    """Registry entry for one named service (may have multiple instances)."""
    name: str
    instances: list[ServiceInstance] = field(default_factory=list)
    friendly_names: list[str] = field(default_factory=list)
    metadata: dict[str, str] = field(default_factory=dict)

    @property
    def healthy_count(self) -> int:
        return sum(1 for i in self.instances if i.healthy)

    def next_instance(self) -> Optional[ServiceInstance]:
        """Round-robin selection among healthy instances."""
        healthy = [i for i in self.instances if i.healthy]
        if not healthy:
            return None
        idx = id(self) % len(healthy)  # simple but deterministic
        return healthy[idx]


# ---------------------------------------------------------------------------
# Route Entry
# ---------------------------------------------------------------------------

@dataclass
class Route:
    """A single route rule."""
    pattern: re.Pattern
    agent_name: str
    strip_prefix: str = "/api"
    methods: list[str] = field(default_factory=lambda: ["GET", "POST", "PUT", "DELETE"])


# ---------------------------------------------------------------------------
# Request Router
# ---------------------------------------------------------------------------

class RequestRouter:
    """Routes incoming requests to the appropriate fleet agent.

    The router maintains a service registry and a route table.  Incoming
    requests are matched against the route table; the first match wins.

    Path format: ``/api/{agent_name}/{path}`` → ``http://{host}:{port}/{path}``
    """

    # Regex for /api/{agent}/{path...}
    API_PATTERN = re.compile(r"^/api/([^/]+)(/.*)?$")

    def __init__(self) -> None:
        self._registry: dict[str, ServiceRegistration] = {}
        self._routes: list[Route] = []
        self._rr_counters: dict[str, int] = {}  # round-robin counters
        self._lock = threading.Lock()

        # Middleware chain
        self._middleware = MiddlewareChain()
        self._timeout_mw = TimeoutMiddleware(default_timeout=30.0)
        self._logging_mw = LoggingMiddleware()
        self._auth_mw: Optional[AuthMiddleware] = None
        self._rate_limiter: Optional[RateLimiter] = None
        self._cors_mw: Optional[CORSMiddleware] = None

    # ---- Service Registry ------------------------------------------------

    def register(
        self,
        name: str,
        host: str,
        port: int,
        friendly_names: Optional[list[str]] = None,
        metadata: Optional[dict[str, str]] = None,
    ) -> None:
        """Register (or add an instance to) a service."""
        with self._lock:
            if name not in self._registry:
                self._registry[name] = ServiceRegistration(
                    name=name,
                    friendly_names=friendly_names or [],
                    metadata=metadata or {},
                )
            reg = self._registry[name]
            # Avoid duplicate instances
            for inst in reg.instances:
                if inst.host == host and inst.port == port:
                    return
            reg.instances.append(ServiceInstance(host=host, port=port))
            logger.info("registered %s → %s:%d", name, host, port)

    def deregister(self, name: str, host: Optional[str] = None, port: Optional[int] = None) -> bool:
        """Remove a service or a specific instance."""
        with self._lock:
            if name not in self._registry:
                return False
            reg = self._registry[name]
            if host is not None and port is not None:
                reg.instances = [
                    i for i in reg.instances
                    if not (i.host == host and i.port == port)
                ]
                if not reg.instances:
                    del self._registry[name]
            else:
                del self._registry[name]
            logger.info("deregistered %s", name)
            return True

    def get_service(self, name: str) -> Optional[ServiceRegistration]:
        return self._registry.get(name)

    def list_services(self) -> dict[str, ServiceRegistration]:
        with self._lock:
            return dict(self._registry)

    def resolve_name(self, name: str) -> Optional[str]:
        """Resolve a friendly name or direct name to the canonical service name."""
        with self._lock:
            # Direct match
            if name in self._registry:
                return name
            # Friendly name lookup
            for svc_name, reg in self._registry.items():
                if name in reg.friendly_names:
                    return svc_name
            return None

    # ---- Route Table -----------------------------------------------------

    def add_route(
        self,
        pattern: str,
        agent_name: str,
        strip_prefix: str = "/api",
        methods: Optional[list[str]] = None,
    ) -> None:
        """Add a custom route pattern."""
        compiled = re.compile(pattern)
        self._routes.append(Route(
            pattern=compiled,
            agent_name=agent_name,
            strip_prefix=strip_prefix,
            methods=methods or ["GET", "POST", "PUT", "DELETE"],
        ))

    def list_routes(self) -> list[dict[str, Any]]:
        """Return all registered routes as serialisable dicts."""
        routes = []
        for r in self._routes:
            routes.append({
                "pattern": r.pattern.pattern,
                "agent": r.agent_name,
                "strip_prefix": r.strip_prefix,
                "methods": r.methods,
            })
        # Built-in API routes
        for name in self._registry:
            routes.append({
                "pattern": f"/api/{name}/<path>",
                "agent": name,
                "strip_prefix": f"/api/{name}",
                "methods": ["GET", "POST", "PUT", "DELETE"],
            })
        return routes

    # ---- Routing Logic ---------------------------------------------------

    def route(self, req: Request) -> Optional[tuple[str, str, str]]:
        """Resolve a request to (base_url, upstream_path, agent_name).

        Returns None if no matching route is found.
        """
        path = req.path

        # 1. Check custom routes first
        for route in self._routes:
            if req.method not in route.methods:
                continue
            m = route.pattern.match(path)
            if m:
                agent_name = route.agent_name
                upstream_path = path[len(route.strip_prefix):]
                if not upstream_path.startswith("/"):
                    upstream_path = "/" + upstream_path
                svc = self._resolve_service(agent_name)
                if svc is None:
                    return None
                return (svc, upstream_path, agent_name)

        # 2. Default /api/{agent}/{path} routing
        m = self.API_PATTERN.match(path)
        if m:
            agent_name = m.group(1)
            upstream_path = m.group(2) or "/"
            canonical = self.resolve_name(agent_name)
            if canonical is None:
                return None
            svc_url = self._round_robin(canonical)
            if svc_url is None:
                return None
            return (svc_url, upstream_path, canonical)

        return None

    def _resolve_service(self, agent_name: str) -> Optional[str]:
        """Resolve agent name to a single backend URL."""
        canonical = self.resolve_name(agent_name)
        if canonical is None:
            return None
        return self._round_robin(canonical)

    def _round_robin(self, canonical_name: str) -> Optional[str]:
        """Round-robin selection among healthy instances."""
        reg = self._registry.get(canonical_name)
        if reg is None or not reg.instances:
            return None

        healthy = [i for i in reg.instances if i.healthy]
        if not healthy:
            # Fall back to any instance if all unhealthy
            healthy = reg.instances

        with self._lock:
            idx = self._rr_counters.get(canonical_name, 0)
            self._rr_counters[canonical_name] = (idx + 1) % len(healthy)
            instance = healthy[idx]

        return instance.base_url

    # ---- Middleware Configuration ----------------------------------------

    def enable_auth(self, secret: str = "fleet-secret-token", skip_paths: Optional[set[str]] = None) -> None:
        self._auth_mw = AuthMiddleware(secret=secret, skip_paths=skip_paths)

    def enable_rate_limiting(self, tokens_per_minute: int = 60, burst_size: int = 10) -> None:
        self._rate_limiter = RateLimiter(tokens_per_minute=tokens_per_minute, burst_size=burst_size)

    def enable_cors(
        self,
        allow_origins: str = "*",
        allow_methods: str = "GET, POST, PUT, DELETE, OPTIONS",
        allow_headers: str = "Content-Type, Authorization",
        max_age: int = 86400,
    ) -> None:
        self._cors_mw = CORSMiddleware(
            allow_origins=allow_origins,
            allow_methods=allow_methods,
            allow_headers=allow_headers,
            max_age=max_age,
        )

    def set_timeout(self, path_prefix: str, timeout: float) -> None:
        self._timeout_mw.set_timeout(path_prefix, timeout)

    def get_timeout(self, path: str) -> float:
        return self._timeout_mw.get_timeout(path)

    @property
    def logging_middleware(self) -> LoggingMiddleware:
        return self._logging_mw

    @property
    def rate_limiter(self) -> Optional[RateLimiter]:
        return self._rate_limiter

    def build_chain(self, handler: Handler) -> Handler:
        """Assemble the full middleware chain around *handler*."""
        chain = MiddlewareChain()
        if self._cors_mw:
            chain.add(self._cors_mw)
        if self._auth_mw:
            chain.add(self._auth_mw)
        if self._rate_limiter:
            chain.add(self._rate_limiter)
        chain.add(self._logging_mw)
        chain.add(self._timeout_mw)
        return chain.build(handler)

    # ---- Health / Stats --------------------------------------------------

    def health_snapshot(self) -> dict[str, Any]:
        """Return health info for all registered services."""
        snapshot = {}
        with self._lock:
            for name, reg in self._registry.items():
                instances = []
                for inst in reg.instances:
                    instances.append({
                        "host": inst.host,
                        "port": inst.port,
                        "healthy": inst.healthy,
                    })
                snapshot[name] = {
                    "healthy": reg.healthy_count > 0,
                    "healthy_instances": reg.healthy_count,
                    "total_instances": len(reg.instances),
                    "instances": instances,
                    "friendly_names": reg.friendly_names,
                }
        return snapshot
