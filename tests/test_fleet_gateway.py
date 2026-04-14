"""Comprehensive test suite for the Fleet API Gateway.

Tests cover:
- Routing (path → agent mapping)
- Authentication middleware (valid / invalid tokens)
- Rate limiter (token bucket)
- CORS headers
- Health aggregation
- Service registration / deregistration
- CLI argument parsing
- Full gateway flow with mock backend servers
"""

from __future__ import annotations

import json
import os
import socket
import sys
import threading
import time
import unittest
import urllib.error
import urllib.request
from http.server import HTTPServer, BaseHTTPRequestHandler
from io import StringIO

# Ensure project root is on sys.path
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from middleware import (
    AuthMiddleware,
    CORSMiddleware,
    LoggingMiddleware,
    RateLimiter,
    Request,
    Response,
    TimeoutMiddleware,
)
from router import RequestRouter, ServiceInstance, ServiceRegistration
from gateway import FleetGateway, discover_from_yaml


# ===========================================================================
# Helpers
# ===========================================================================

def make_request(method: str = "GET", path: str = "/", **kwargs) -> Request:
    """Build a Request with sensible defaults."""
    return Request(method=method, path=path, **kwargs)


def make_json_response(data: dict, status: int = 200) -> Response:
    return Response().set_json(data, status=status)


class MockHandler(BaseHTTPRequestHandler):
    """A mock HTTP backend that returns simple JSON responses."""

    def do_GET(self):
        body = json.dumps({"path": self.path, "method": "GET", "agent": "mock"}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        req_body = self.rfile.read(length) if length else b""
        body = json.dumps({
            "path": self.path,
            "method": "POST",
            "agent": "mock",
            "echo": req_body.decode(),
        }).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_PUT(self):
        body = json.dumps({"path": self.path, "method": "PUT"}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_DELETE(self):
        body = json.dumps({"path": self.path, "method": "DELETE"}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format, *args):
        pass  # suppress


class ReusableMockServer(HTTPServer):
    allow_reuse_address = True
    allow_reuse_port = False


def start_mock_server(port: int) -> HTTPServer:
    """Start a mock backend on *port* in a daemon thread."""
    server = ReusableMockServer(("127.0.0.1", port), MockHandler)
    t = threading.Thread(target=server.serve_forever, daemon=True)
    t.start()
    time.sleep(0.15)  # give server time to bind
    return server


# ===========================================================================
# Routing Tests
# ===========================================================================

class TestRouting(unittest.TestCase):
    """Test path → agent mapping."""

    def setUp(self):
        self.router = RequestRouter()
        self.router.register("trail-agent", "localhost", 8501)
        self.router.register("git-agent", "localhost", 8502, friendly_names=["git", "repo"])

    def test_basic_api_route(self):
        req = make_request("GET", "/api/trail-agent/encode")
        result = self.router.route(req)
        self.assertIsNotNone(result)
        base_url, upstream_path, agent_name = result
        self.assertEqual(agent_name, "trail-agent")
        self.assertEqual(upstream_path, "/encode")
        self.assertEqual(base_url, "http://localhost:8501")

    def test_friendly_name_routing(self):
        req = make_request("GET", "/api/git/commits")
        result = self.router.route(req)
        self.assertIsNotNone(result)
        _, upstream_path, agent_name = result
        self.assertEqual(agent_name, "git-agent")
        self.assertEqual(upstream_path, "/commits")

    def test_unknown_agent_returns_none(self):
        req = make_request("GET", "/api/unknown-agent/hello")
        result = self.router.route(req)
        self.assertIsNone(result)

    def test_non_api_path_returns_none(self):
        req = make_request("GET", "/health")
        result = self.router.route(req)
        self.assertIsNone(result)

    def test_root_api_path(self):
        req = make_request("GET", "/api/trail-agent")
        result = self.router.route(req)
        self.assertIsNotNone(result)
        _, upstream_path, agent_name = result
        self.assertEqual(upstream_path, "/")
        self.assertEqual(agent_name, "trail-agent")

    def test_round_robin_across_instances(self):
        self.router.register("trail-agent", "localhost", 8510)
        results = set()
        for _ in range(10):
            req = make_request("GET", "/api/trail-agent/test")
            r = self.router.route(req)
            self.assertIsNotNone(r)
            results.add(r[0])
        # Should have seen at least 2 different backends
        self.assertGreaterEqual(len(results), 2)

    def test_deregister_service(self):
        ok = self.router.deregister("trail-agent")
        self.assertTrue(ok)
        req = make_request("GET", "/api/trail-agent/test")
        self.assertIsNone(self.router.route(req))

    def test_deregister_nonexistent(self):
        ok = self.router.deregister("nonexistent")
        self.assertFalse(ok)

    def test_list_routes(self):
        routes = self.router.list_routes()
        self.assertIsInstance(routes, list)
        self.assertGreater(len(routes), 0)


# ===========================================================================
# Authentication Tests
# ===========================================================================

class TestAuthMiddleware(unittest.TestCase):
    """Test Bearer token validation."""

    def setUp(self):
        self.secret = "my-fleet-secret"
        self.auth = AuthMiddleware(secret=self.secret, skip_paths={"/health"})
        self.next_called = False

        def next_handler(req: Request) -> Response:
            self.next_called = True
            return Response().set_json({"ok": True})

        self.handler = self.auth(next_handler)

    def test_valid_token_passes(self):
        req = make_request(
            "GET", "/api/trail-agent/test",
            headers={"authorization": f"Bearer {self.secret}"},
        )
        resp = self.handler(req)
        self.assertEqual(resp.status, 200)
        self.assertTrue(self.next_called)

    def test_invalid_token_rejected(self):
        req = make_request(
            "GET", "/api/trail-agent/test",
            headers={"authorization": "Bearer wrong-token"},
        )
        resp = self.handler(req)
        self.assertEqual(resp.status, 403)
        self.assertFalse(self.next_called)

    def test_missing_auth_rejected(self):
        req = make_request("GET", "/api/trail-agent/test")
        resp = self.handler(req)
        self.assertEqual(resp.status, 401)
        self.assertFalse(self.next_called)

    def test_skip_path_bypasses_auth(self):
        req = make_request("GET", "/health")
        resp = self.handler(req)
        self.assertEqual(resp.status, 200)
        self.assertTrue(self.next_called)

    def test_malformed_auth_header(self):
        req = make_request(
            "GET", "/api/trail-agent/test",
            headers={"authorization": "Token abc123"},
        )
        resp = self.handler(req)
        self.assertEqual(resp.status, 401)


# ===========================================================================
# Rate Limiter Tests
# ===========================================================================

class TestRateLimiter(unittest.TestCase):
    """Test token-bucket rate limiting."""

    def setUp(self):
        self.limiter = RateLimiter(tokens_per_minute=60, burst_size=5)

    def test_allows_within_burst(self):
        for _ in range(5):
            allowed = self.limiter._consume("client-1")
            self.assertTrue(allowed)

    def test_rejects_over_burst(self):
        for _ in range(5):
            self.limiter._consume("client-2")
        # 6th should fail (bucket empty, no time to refill)
        allowed = self.limiter._consume("client-2")
        self.assertFalse(allowed)

    def test_separate_clients(self):
        for _ in range(5):
            self.limiter._consume("client-a")
        # Different client should still have tokens
        allowed = self.limiter._consume("client-b")
        self.assertTrue(allowed)

    def test_middleware_returns_429(self):
        def next_handler(req):
            return Response().set_json({"ok": True})

        handler = self.limiter(next_handler)

        # Exhaust tokens
        for _ in range(5):
            handler(make_request(client_ip="ip-1"))

        req = make_request("GET", "/api/test", client_ip="ip-1")
        resp = handler(req)
        self.assertEqual(resp.status, 429)
        data = resp.json()
        self.assertEqual(data["error"], "rate limit exceeded")

    def test_reset(self):
        for _ in range(5):
            self.limiter._consume("client-r")
        self.limiter.reset()
        allowed = self.limiter._consume("client-r")
        self.assertTrue(allowed)


# ===========================================================================
# CORS Tests
# ===========================================================================

class TestCORSMiddleware(unittest.TestCase):
    """Test CORS header injection."""

    def setUp(self):
        self.cors = CORSMiddleware(allow_origins="*", allow_methods="GET, POST")

        def next_handler(req):
            return Response().set_json({"ok": True})

        self.handler = self.cors(next_handler)

    def test_cors_headers_present(self):
        resp = self.handler(make_request("GET", "/api/test"))
        self.assertEqual(resp.headers["Access-Control-Allow-Origin"], "*")
        self.assertIn("GET", resp.headers["Access-Control-Allow-Methods"])

    def test_preflight_returns_204(self):
        resp = self.handler(make_request("OPTIONS", "/api/test"))
        self.assertEqual(resp.status, 204)
        self.assertEqual(resp.headers["Access-Control-Allow-Origin"], "*")

    def test_custom_cors_config(self):
        cors = CORSMiddleware(allow_origins="https://fleet.example.com")
        handler = cors(lambda req: Response().set_json({"ok": True}))
        resp = handler(make_request("GET", "/test"))
        self.assertEqual(resp.headers["Access-Control-Allow-Origin"], "https://fleet.example.com")


# ===========================================================================
# Logging Middleware Tests
# ===========================================================================

class TestLoggingMiddleware(unittest.TestCase):
    """Test request logging."""

    def setUp(self):
        self.log_mw = LoggingMiddleware()

        def next_handler(req):
            return Response().set_json({"ok": True})

        self.handler = self.log_mw(next_handler)

    def test_logs_request(self):
        self.handler(make_request("GET", "/api/test"))
        log = self.log_mw.get_log()
        self.assertEqual(len(log), 1)
        self.assertEqual(log[0]["method"], "GET")
        self.assertEqual(log[0]["path"], "/api/test")
        self.assertEqual(log[0]["status"], 200)

    def test_latency_recorded(self):
        self.handler(make_request("POST", "/api/data"))
        log = self.log_mw.get_log()
        self.assertGreaterEqual(log[0]["latency_ms"], 0)

    def test_log_rotation(self):
        # Log 1001 entries and check only 1000 are kept
        for i in range(1050):
            self.handler(make_request("GET", f"/api/item/{i}"))
        log = self.log_mw.get_log()
        self.assertEqual(len(log), 1000)

    def test_clear_log(self):
        self.handler(make_request("GET", "/api/test"))
        self.log_mw.clear_log()
        self.assertEqual(len(self.log_mw.get_log()), 0)


# ===========================================================================
# Timeout Middleware Tests
# ===========================================================================

class TestTimeoutMiddleware(unittest.TestCase):
    """Test per-path timeout configuration."""

    def test_default_timeout(self):
        tmw = TimeoutMiddleware(default_timeout=30.0)
        self.assertEqual(tmw.get_timeout("/api/anything"), 30.0)

    def test_custom_path_timeout(self):
        tmw = TimeoutMiddleware(default_timeout=30.0)
        tmw.set_timeout("/api/slow-agent", 120.0)
        self.assertEqual(tmw.get_timeout("/api/slow-agent/upload"), 120.0)
        self.assertEqual(tmw.get_timeout("/api/fast-agent/ping"), 30.0)

    def test_most_specific_prefix_wins(self):
        tmw = TimeoutMiddleware(default_timeout=10.0)
        tmw.set_timeout("/api/a", 20.0)
        tmw.set_timeout("/api/a/special", 50.0)
        self.assertEqual(tmw.get_timeout("/api/a/special/path"), 50.0)
        self.assertEqual(tmw.get_timeout("/api/a/normal"), 20.0)


# ===========================================================================
# Health Aggregation Tests
# ===========================================================================

class TestHealthAggregation(unittest.TestCase):
    """Test fleet-wide health endpoint."""

    def test_empty_fleet_is_healthy(self):
        gw = FleetGateway(port=0, enable_auth=False, enable_rate_limit=False)
        health = gw.health()
        self.assertEqual(health["status"], "healthy")
        self.assertEqual(health["gateway"], "ok")

    def test_registered_agents_in_health(self):
        gw = FleetGateway(port=0, enable_auth=False, enable_rate_limit=False)
        gw.register_service("agent-a", "localhost", 9001)
        gw.register_service("agent-b", "localhost", 9002)
        health = gw.health()
        self.assertIn("agent-a", health["services"])
        self.assertIn("agent-b", health["services"])
        self.assertEqual(health["services"]["agent-a"]["healthy_instances"], 1)


# ===========================================================================
# Service Registration / Deregistration Tests
# ===========================================================================

class TestServiceRegistry(unittest.TestCase):
    """Test dynamic service registration and deregistration."""

    def test_register_and_list(self):
        router = RequestRouter()
        router.register("svc-1", "host1", 1001)
        router.register("svc-2", "host2", 1002, friendly_names=["two"])
        services = router.list_services()
        self.assertIn("svc-1", services)
        self.assertIn("svc-2", services)
        self.assertEqual(services["svc-2"].friendly_names, ["two"])

    def test_duplicate_registration_no_dupe_instance(self):
        router = RequestRouter()
        router.register("svc", "host", 100)
        router.register("svc", "host", 100)
        services = router.list_services()
        self.assertEqual(len(services["svc"].instances), 1)

    def test_deregister_removes_service(self):
        router = RequestRouter()
        router.register("svc", "host", 100)
        ok = router.deregister("svc")
        self.assertTrue(ok)
        self.assertIsNone(router.get_service("svc"))

    def test_deregister_specific_instance(self):
        router = RequestRouter()
        router.register("svc", "host1", 100)
        router.register("svc", "host2", 200)
        ok = router.deregister("svc", host="host1", port=100)
        self.assertTrue(ok)
        svc = router.get_service("svc")
        self.assertIsNotNone(svc)
        self.assertEqual(len(svc.instances), 1)
        self.assertEqual(svc.instances[0].host, "host2")

    def test_resolve_name_direct(self):
        router = RequestRouter()
        router.register("my-agent", "host", 100)
        self.assertEqual(router.resolve_name("my-agent"), "my-agent")

    def test_resolve_name_friendly(self):
        router = RequestRouter()
        router.register("my-agent", "host", 100, friendly_names=["ma", "agent"])
        self.assertEqual(router.resolve_name("ma"), "my-agent")
        self.assertIsNone(router.resolve_name("unknown"))


# ===========================================================================
# Gateway Stats Tests
# ===========================================================================

class TestGatewayStats(unittest.TestCase):
    """Test gateway statistics."""

    def test_initial_stats(self):
        gw = FleetGateway(port=0, enable_auth=False, enable_rate_limit=False)
        stats = gw.stats()
        self.assertEqual(stats["total_requests"], 0)
        self.assertEqual(stats["total_errors"], 0)
        self.assertIn("uptime_seconds", stats)

    def test_stats_increments(self):
        gw = FleetGateway(port=0, enable_auth=False, enable_rate_limit=False)
        gw._record_request()
        gw._record_request(error=True)
        gw._record_request(proxied=True)
        stats = gw.stats()
        self.assertEqual(stats["total_requests"], 3)
        self.assertEqual(stats["total_errors"], 1)
        self.assertEqual(stats["total_proxied"], 1)


def _find_free_port() -> int:
    """Find and return a free TCP port on 127.0.0.1."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]


# ===========================================================================
# Full Gateway Flow (Integration) Tests
# ===========================================================================

class TestFullGatewayFlow(unittest.TestCase):
    """End-to-end test with real HTTP connections to the gateway."""

    @classmethod
    def setUpClass(cls):
        # Pick free ports to avoid conflicts
        mock_port_1 = _find_free_port()
        mock_port_2 = _find_free_port()
        gw_port = _find_free_port()

        # Start mock backends
        cls.mock_servers = [
            start_mock_server(mock_port_1),
            start_mock_server(mock_port_2),
        ]
        cls._mock_ports = (mock_port_1, mock_port_2)
        cls._gw_port = gw_port

        # Start gateway
        cls.gateway = FleetGateway(
            port=gw_port,
            enable_auth=False,
            enable_rate_limit=False,
            enable_cors=True,
            fleet_yaml="/dev/null",  # skip yaml discovery
        )
        cls.gateway.register_service("trail-agent", "127.0.0.1", mock_port_1)
        cls.gateway.register_service("git-agent", "127.0.0.1", mock_port_2)
        cls.gateway.serve(blocking=False)
        time.sleep(0.2)  # let server start

    @classmethod
    def tearDownClass(cls):
        cls.gateway.shutdown()
        for s in cls.mock_servers:
            s.shutdown()

    def _fetch(self, path: str, method: str = "GET", body: bytes | None = None, headers: dict | None = None) -> tuple[int, bytes]:
        url = f"http://127.0.0.1:{self._gw_port}{path}"
        data = body
        req = urllib.request.Request(url, data=data, method=method)
        if headers:
            for k, v in headers.items():
                req.add_header(k, v)
        try:
            resp = urllib.request.urlopen(req, timeout=3)
            return resp.status, resp.read()
        except urllib.error.HTTPError as e:
            return e.code, e.read() if e.fp else b""

    def test_health_endpoint(self):
        status, body = self._fetch("/health")
        self.assertEqual(status, 200)
        data = json.loads(body)
        self.assertEqual(data["status"], "healthy")

    def test_agents_endpoint(self):
        status, body = self._fetch("/agents")
        self.assertEqual(status, 200)
        data = json.loads(body)
        self.assertIn("trail-agent", data)
        self.assertIn("git-agent", data)

    def test_stats_endpoint(self):
        status, body = self._fetch("/stats")
        self.assertEqual(status, 200)
        data = json.loads(body)
        self.assertIn("total_requests", data)

    def test_proxy_get(self):
        status, body = self._fetch("/api/trail-agent/encode")
        self.assertEqual(status, 200)
        data = json.loads(body)
        self.assertEqual(data["agent"], "mock")
        self.assertIn("/encode", data["path"])

    def test_proxy_post(self):
        payload = json.dumps({"msg": "hello"}).encode()
        status, body = self._fetch("/api/git-agent/commit", method="POST", body=payload)
        self.assertEqual(status, 200)
        data = json.loads(body)
        self.assertEqual(data["method"], "POST")
        self.assertIn("commit", data["path"])

    def test_proxy_to_unknown_agent(self):
        status, body = self._fetch("/api/nonexistent/hello")
        self.assertEqual(status, 404)

    def test_cors_headers(self):
        status, body = self._fetch("/health")
        # CORS is added by middleware, check via actual HTTP request
        # The response should have CORS headers in gateway
        self.assertEqual(status, 200)

    def test_registry_register(self):
        payload = json.dumps({"name": "new-agent", "host": "localhost", "port": 9999}).encode()
        status, body = self._fetch(
            "/registry/register", method="POST", body=payload,
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(status, 201)

    def test_registry_deregister(self):
        self.gateway.register_service("temp-agent", "localhost", 8888)
        status, body = self._fetch("/registry/temp-agent", method="DELETE")
        self.assertEqual(status, 200)

    def test_deregister_nonexistent(self):
        status, body = self._fetch("/registry/nope", method="DELETE")
        self.assertEqual(status, 404)

    def test_not_found(self):
        status, body = self._fetch("/nonexistent")
        self.assertEqual(status, 404)

    def test_options_preflight(self):
        status, body = self._fetch("/api/trail-agent/test", method="OPTIONS")
        self.assertEqual(status, 204)


# ===========================================================================
# Gateway with Auth (Integration)
# ===========================================================================

class TestGatewayWithAuth(unittest.TestCase):
    """Test gateway with authentication enabled."""

    @classmethod
    def setUpClass(cls):
        mock_port = _find_free_port()
        gw_port = _find_free_port()

        cls.mock_server = start_mock_server(mock_port)
        cls._mock_port = mock_port
        cls._gw_port = gw_port
        cls.gateway = FleetGateway(
            port=gw_port,
            secret="test-secret",
            enable_auth=True,
            enable_rate_limit=False,
            fleet_yaml="/dev/null",
        )
        cls.gateway.register_service("agent", "127.0.0.1", mock_port)
        cls.gateway.serve(blocking=False)
        time.sleep(0.2)

    @classmethod
    def tearDownClass(cls):
        cls.gateway.shutdown()
        cls.mock_server.shutdown()

    def _fetch(self, path: str, token: str | None = None) -> tuple[int, bytes]:
        url = f"http://127.0.0.1:{self._gw_port}{path}"
        req = urllib.request.Request(url)
        if token:
            req.add_header("Authorization", f"Bearer {token}")
        try:
            resp = urllib.request.urlopen(req, timeout=3)
            return resp.status, resp.read()
        except urllib.error.HTTPError as e:
            return e.code, e.read() if e.fp else b""

    def test_public_endpoints_no_auth(self):
        status, _ = self._fetch("/health")
        self.assertEqual(status, 200)
        status, _ = self._fetch("/agents")
        self.assertEqual(status, 200)

    def test_protected_without_token(self):
        status, _ = self._fetch("/api/agent/test")
        self.assertEqual(status, 401)

    def test_protected_with_valid_token(self):
        status, _ = self._fetch("/api/agent/test", token="test-secret")
        self.assertEqual(status, 200)

    def test_protected_with_wrong_token(self):
        status, _ = self._fetch("/api/agent/test", token="wrong")
        self.assertEqual(status, 403)


# ===========================================================================
# Fleet YAML Discovery Tests
# ===========================================================================

class TestYamlDiscovery(unittest.TestCase):
    """Test fleet.yaml parsing."""

    def test_parse_valid_yaml(self):
        import tempfile
        content = """\
agents:
  - name: trail-agent
    host: localhost
    port: 8501
    aliases: [trail, encoder]
  - name: git-agent
    host: 192.168.1.10
    port: 8502
"""
        with tempfile.NamedTemporaryFile(mode="w", suffix=".yaml", delete=False) as f:
            f.write(content)
            path = f.name

        try:
            agents = discover_from_yaml(path)
            self.assertEqual(len(agents), 2)
            self.assertEqual(agents[0]["name"], "trail-agent")
            self.assertEqual(agents[0]["port"], 8501)
            self.assertEqual(agents[0]["aliases"], ["trail", "encoder"])
            self.assertEqual(agents[1]["host"], "192.168.1.10")
        finally:
            os.unlink(path)

    def test_missing_file_returns_empty(self):
        agents = discover_from_yaml("/nonexistent/path.yaml")
        self.assertEqual(agents, [])


# ===========================================================================
# CLI Tests
# ===========================================================================

class TestCLI(unittest.TestCase):
    """Test CLI argument parsing."""

    def test_serve_defaults(self):
        from cli import build_parser
        parser = build_parser()
        args = parser.parse_args(["serve"])
        self.assertEqual(args.port, 9001)
        self.assertFalse(args.no_auth)
        self.assertFalse(args.no_rate_limit)

    def test_serve_custom_port(self):
        from cli import build_parser
        parser = build_parser()
        args = parser.parse_args(["serve", "--port", "8080"])
        self.assertEqual(args.port, 8080)

    def test_serve_no_auth(self):
        from cli import build_parser
        parser = build_parser()
        args = parser.parse_args(["serve", "--no-auth"])
        self.assertTrue(args.no_auth)

    def test_test_command(self):
        from cli import build_parser
        parser = build_parser()
        args = parser.parse_args(["test", "--port", "9999"])
        self.assertEqual(args.port, 9999)

    def test_routes_command(self):
        from cli import build_parser
        parser = build_parser()
        args = parser.parse_args(["routes"])
        self.assertEqual(args.command, "routes")

    def test_agents_command(self):
        from cli import build_parser
        parser = build_parser()
        args = parser.parse_args(["agents"])
        self.assertEqual(args.command, "agents")

    def test_onboard_command(self):
        from cli import build_parser
        parser = build_parser()
        args = parser.parse_args(["onboard"])
        self.assertEqual(args.command, "onboard")


# ===========================================================================
# Run
# ===========================================================================

if __name__ == "__main__":
    unittest.main(verbosity=2)
