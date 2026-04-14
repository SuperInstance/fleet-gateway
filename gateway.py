"""Fleet API Gateway — unified HTTP interface to the Pelagic fleet.

Single entry point: ``http://localhost:9001/``
Routes to: keeper, git-agent, trail-agent, trust-agent, etc.

Features
--------
* Service Discovery — auto-discover fleet agents from fleet.yaml or registry
* Request Routing  — ``/api/{agent}/{endpoint}`` → routes to the right agent
* Load Balancing   — round-robin if multiple instances of an agent
* Rate Limiting    — per-client token-bucket rate limiter
* Authentication   — Bearer token auth (shared fleet secret)
* Request Logging  — log all requests for audit
* Health Aggregation — ``/health`` returns fleet-wide health
* CORS            — configurable CORS headers
* Timeout         — configurable per-service timeout
* Retry           — automatic retry on 5xx errors (1 retry)

Uses only the Python standard library.
"""

from __future__ import annotations

import json
import logging
import os
import re
import socket
import ssl
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from http.server import HTTPServer, BaseHTTPRequestHandler
from typing import Any, Optional

from middleware import Request, Response
from router import RequestRouter

logger = logging.getLogger("fleet.gateway")

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

DEFAULT_PORT = 9001
DEFAULT_FLEET_SECRET = "fleet-secret-token"
DEFAULT_RATE_LIMIT = 60  # tokens per minute


class ReusableHTTPServer(HTTPServer):
    """HTTPServer with SO_REUSEADDR enabled."""
    allow_reuse_address = True
    allow_reuse_port = False


# ---------------------------------------------------------------------------
# Fleet YAML Discovery
# ---------------------------------------------------------------------------

def discover_from_yaml(path: str = "fleet.yaml") -> list[dict[str, Any]]:
    """Parse a simple fleet.yaml to discover agents.

    Expected format (YAML-like, parsed with simple regex):

        agents:
          - name: trail-agent
            host: localhost
            port: 8501
            aliases: [trail, encoder]
          - name: git-agent
            host: localhost
            port: 8502
    """
    if not os.path.isfile(path):
        logger.info("no fleet.yaml found at %s", path)
        return []

    agents: list[dict[str, Any]] = []
    current_agent: Optional[dict[str, Any]] = None

    with open(path, "r") as f:
        for raw_line in f:
            line = raw_line.strip()
            if not line or line.startswith("#"):
                continue

            # Detect agent entry: "  - name: trail-agent"
            m = re.match(r"^\s*-\s*name:\s*(.+)$", line)
            if m:
                if current_agent:
                    agents.append(current_agent)
                current_agent = {"name": m.group(1).strip(), "host": "localhost", "port": 8000}
                continue

            if current_agent is None:
                continue

            m = re.match(r"^\s*host:\s*(.+)$", line)
            if m:
                current_agent["host"] = m.group(1).strip()
                continue

            m = re.match(r"^\s*port:\s*(\d+)$", line)
            if m:
                current_agent["port"] = int(m.group(1))
                continue

            m = re.match(r"^\s*aliases:\s*\[(.+)\]$", line)
            if m:
                aliases = [a.strip().strip('"\'') for a in m.group(1).split(",")]
                current_agent["aliases"] = aliases
                continue

    if current_agent:
        agents.append(current_agent)

    logger.info("discovered %d agents from %s", len(agents), path)
    return agents


# ---------------------------------------------------------------------------
# Fleet Gateway
# ---------------------------------------------------------------------------

class FleetGateway:
    """Unified API gateway for the Pelagic fleet.

    Single entry point: ``http://localhost:9001/``
    Routes to: keeper, git-agent, trail-agent, trust-agent, etc.
    """

    def __init__(
        self,
        port: int = DEFAULT_PORT,
        secret: str = DEFAULT_FLEET_SECRET,
        rate_limit: int = DEFAULT_RATE_LIMIT,
        enable_auth: bool = True,
        enable_rate_limit: bool = True,
        enable_cors: bool = True,
        fleet_yaml: str = "fleet.yaml",
    ) -> None:
        self.port = port
        self.secret = secret
        self.rate_limit = rate_limit
        self.enable_auth = enable_auth
        self.enable_rate_limit = enable_rate_limit
        self.enable_cors_flag = enable_cors

        self.router = RequestRouter()
        self._server: Optional[HTTPServer] = None
        self._thread: Optional[threading.Thread] = None

        # Statistics
        self._stats_lock = threading.Lock()
        self._stats = {
            "total_requests": 0,
            "total_errors": 0,
            "total_proxied": 0,
            "start_time": time.time(),
        }

        # Configure middleware via router
        if enable_auth:
            self.router.enable_auth(secret=secret)
        if enable_rate_limit:
            self.router.enable_rate_limiting(tokens_per_minute=rate_limit)
        if enable_cors:
            self.router.enable_cors()

        # Auto-discover agents
        self._discovered = discover_from_yaml(fleet_yaml)
        for agent in self._discovered:
            self.router.register(
                name=agent["name"],
                host=agent["host"],
                port=agent["port"],
                friendly_names=agent.get("aliases"),
            )

    # ---- Service Discovery -----------------------------------------------

    def discover_agents(self) -> list[dict[str, Any]]:
        """Return the list of discovered agents."""
        return self._discovered

    def register_service(
        self,
        name: str,
        host: str,
        port: int,
        friendly_names: Optional[list[str]] = None,
        metadata: Optional[dict[str, str]] = None,
    ) -> None:
        """Programmatically register a service."""
        self.router.register(name, host, port, friendly_names, metadata)

    def deregister_service(self, name: str) -> bool:
        """Programmatically deregister a service."""
        return self.router.deregister(name)

    # ---- Health ----------------------------------------------------------

    def health(self) -> dict[str, Any]:
        """Return fleet-wide health aggregation."""
        services = self.router.health_snapshot()
        all_healthy = all(
            svc["healthy"] for svc in services.values()
        ) if services else True
        return {
            "status": "healthy" if all_healthy else "degraded",
            "gateway": "ok",
            "uptime_seconds": round(time.time() - self._stats["start_time"], 1),
            "services": services,
        }

    # ---- Registry Endpoints ----------------------------------------------

    def registry(self) -> dict[str, Any]:
        """Return the full service registry."""
        services = self.router.list_services()
        result = {}
        for name, reg in services.items():
            result[name] = {
                "instances": [
                    {"host": i.host, "port": i.port, "healthy": i.healthy}
                    for i in reg.instances
                ],
                "friendly_names": reg.friendly_names,
                "metadata": reg.metadata,
            }
        return result

    def registry_register(self, data: dict[str, Any]) -> Response:
        """Register a new service via the API."""
        name = data.get("name")
        host = data.get("host", "localhost")
        port = data.get("port")
        if not name or not port:
            return Response(status=400).set_json({
                "error": "'name' and 'port' are required",
            })
        self.router.register(
            name=name,
            host=host,
            port=int(port),
            friendly_names=data.get("friendly_names"),
            metadata=data.get("metadata"),
        )
        return Response(status=201).set_json({"message": f"registered {name}"})

    # ---- Statistics ------------------------------------------------------

    def stats(self) -> dict[str, Any]:
        """Return gateway statistics."""
        with self._stats_lock:
            s = dict(self._stats)
        s["uptime_seconds"] = round(time.time() - s["start_time"], 1)
        s["registered_agents"] = len(self.router.list_services())
        s["routes"] = len(self.router.list_routes())
        return s

    def _record_request(self, error: bool = False, proxied: bool = False) -> None:
        with self._stats_lock:
            self._stats["total_requests"] += 1
            if error:
                self._stats["total_errors"] += 1
            if proxied:
                self._stats["total_proxied"] += 1

    # ---- Proxy -----------------------------------------------------------

    def _proxy_request(self, req: Request) -> Response:
        """Proxy a request to the appropriate fleet agent."""
        result = self.router.route(req)
        if result is None:
            return Response(status=404).set_json({
                "error": "no route found",
                "path": req.path,
            })

        base_url, upstream_path, agent_name = result
        target_url = f"{base_url}{upstream_path}"

        # Append query string if present
        if "?" in req.path:
            qs = req.path.split("?", 1)[1]
            target_url += f"?{qs}"

        timeout = self.router.get_timeout(req.path)
        return self._do_proxy(
            target_url=target_url,
            method=req.method,
            headers=req.headers,
            body=req.body,
            timeout=timeout,
            agent_name=agent_name,
        )

    def _do_proxy(
        self,
        target_url: str,
        method: str,
        headers: dict[str, str],
        body: Optional[bytes],
        timeout: float,
        agent_name: str,
    ) -> Response:
        """Execute the proxied HTTP request with retry on 5xx."""
        last_error: Optional[Exception] = None

        for attempt in range(2):  # 1 initial + 1 retry
            try:
                req_body = body if body else None
                proxy_req = urllib.request.Request(
                    target_url,
                    data=req_body if method in ("POST", "PUT") else None,
                    method=method,
                )

                # Forward relevant headers
                hop_by_hop = {
                    "host", "connection", "keep-alive",
                    "transfer-encoding", "te", "trailer",
                    "upgrade", "proxy-authorization",
                }
                for k, v in headers.items():
                    if k.lower() not in hop_by_hop:
                        proxy_req.add_header(k, v)

                resp = urllib.request.urlopen(proxy_req, timeout=timeout)
                resp_body = resp.read()
                resp_headers = {
                    k: v for k, v in resp.getheaders() if k.lower() != "transfer-encoding"
                }
                self._record_request(proxied=True)
                return Response(status=resp.status, body=resp_body, headers=resp_headers)

            except urllib.error.HTTPError as e:
                last_error = e
                if 500 <= e.code < 600 and attempt == 0:
                    logger.warning(
                        "5xx from %s (%d), retrying…", agent_name, e.code,
                    )
                    time.sleep(0.1)
                    continue
                resp_body = e.read() if e.fp else b""
                self._record_request(error=True)
                return Response(
                    status=e.code,
                    body=resp_body,
                    headers={"content-type": "application/json"},
                )

            except (urllib.error.URLError, socket.timeout, OSError) as e:
                last_error = e
                logger.error("upstream error for %s: %s", agent_name, e)
                if attempt == 0:
                    time.sleep(0.1)
                    continue
                self._record_request(error=True)
                return Response(status=502).set_json({
                    "error": "upstream unavailable",
                    "agent": agent_name,
                    "detail": str(last_error),
                })

        # Should not reach here, but just in case
        self._record_request(error=True)
        return Response(status=502).set_json({
            "error": "upstream unavailable after retries",
            "agent": agent_name,
        })

    # ---- Request Handler -------------------------------------------------

    def handle_request(self, req: Request) -> Response:
        """Main request dispatcher — called by the HTTP handler."""
        path = req.path.split("?")[0]  # strip query string for routing

        # Built-in endpoints
        if path == "/health":
            return Response().set_json(self.health())
        if path == "/agents":
            return Response().set_json(self.registry())
        if path == "/registry":
            return Response().set_json(self.registry())
        if path == "/registry/register" and req.method == "POST":
            try:
                data = json.loads(req.body) if req.body else {}
            except (json.JSONDecodeError, TypeError):
                return Response(status=400).set_json({"error": "invalid json"})
            return self.registry_register(data)
        if path == "/stats":
            return Response().set_json(self.stats())

        # Deregister: DELETE /registry/{name}
        m = re.match(r"^/registry/([^/]+)$", path)
        if m and req.method == "DELETE":
            name = m.group(1)
            ok = self.deregister_service(name)
            if ok:
                return Response().set_json({"message": f"deregistered {name}"})
            return Response(status=404).set_json({"error": f"service '{name}' not found"})

        # Proxy all /api/* requests
        if path.startswith("/api/"):
            return self._proxy_request(req)

        # Unknown path
        return Response(status=404).set_json({
            "error": "not found",
            "path": req.path,
            "available": ["/health", "/agents", "/registry", "/stats", "/api/{agent}/{path}"],
        })

    # ---- HTTP Server -----------------------------------------------------

    def _make_handler(self):
        """Create a BaseHTTPRequestHandler subclass bound to this gateway."""

        gateway = self
        router_ref = self.router

        class GatewayHandler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, format, *args):
                # Suppress default logging — our middleware handles it
                pass

            def _build_request(self) -> Request:
                content_length = int(self.headers.get("Content-Length", 0))
                body = self.rfile.read(content_length) if content_length > 0 else None
                # Normalise header keys to lower-case for consistent look-ups
                headers = {k.lower(): v for k, v in self.headers.items()}
                client_ip = self.client_address[0] if self.client_address else "unknown"
                return Request(
                    method=self.command,
                    path=self.path,
                    headers=headers,
                    body=body,
                    client_ip=client_ip,
                )

            def _send_response(self, resp: Response) -> None:
                self.send_response(resp.status)
                for k, v in resp.headers.items():
                    self.send_header(k, v)
                if "content-length" not in resp.headers:
                    body = resp.body if isinstance(resp.body, (bytes, bytearray)) else resp.body.encode()
                    self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                if resp.body:
                    body = resp.body if isinstance(resp.body, (bytes, bytearray)) else resp.body.encode()
                    self.wfile.write(body)

            def do_GET(self):
                self._dispatch()

            def do_POST(self):
                self._dispatch()

            def do_PUT(self):
                self._dispatch()

            def do_DELETE(self):
                self._dispatch()

            def do_OPTIONS(self):
                self._dispatch()

            def _dispatch(self):
                req = self._build_request()
                handler = router_ref.build_chain(gateway.handle_request)
                resp = handler(req)
                self._send_response(resp)

        return GatewayHandler

    def serve(self, blocking: bool = True) -> None:
        """Start the gateway HTTP server."""
        handler_cls = self._make_handler()
        self._server = ReusableHTTPServer(("0.0.0.0", self.port), handler_cls)
        logger.info("Fleet Gateway listening on port %d", self.port)

        if blocking:
            try:
                self._server.serve_forever()
            except KeyboardInterrupt:
                logger.info("shutting down gateway")
                self._server.shutdown()
        else:
            self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)
            self._thread.start()

    def shutdown(self) -> None:
        """Stop the gateway server."""
        if self._server:
            self._server.shutdown()
            logger.info("gateway stopped")


# ---------------------------------------------------------------------------
# Convenience
# ---------------------------------------------------------------------------

def create_gateway(
    port: int = DEFAULT_PORT,
    secret: str = DEFAULT_FLEET_SECRET,
    rate_limit: int = DEFAULT_RATE_LIMIT,
    fleet_yaml: str = "fleet.yaml",
) -> FleetGateway:
    """Create and return a configured FleetGateway."""
    return FleetGateway(
        port=port,
        secret=secret,
        rate_limit=rate_limit,
        fleet_yaml=fleet_yaml,
    )


if __name__ == "__main__":
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s [%(name)s] %(levelname)s: %(message)s",
    )
    gw = create_gateway()
    gw.serve()
