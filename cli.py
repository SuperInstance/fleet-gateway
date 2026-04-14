#!/usr/bin/env python3
"""Fleet Gateway CLI — manage and run the API gateway.

Usage:
    python cli.py serve [--port PORT] [--secret SECRET] [--no-auth] [--no-rate-limit]
    python cli.py routes
    python cli.py agents
    python cli.py test [--port PORT]
    python cli.py onboard
"""

from __future__ import annotations

import argparse
import json
import logging
import socket
import sys
import time
import urllib.error
import urllib.request
from typing import Optional

from gateway import DEFAULT_PORT, DEFAULT_FLEET_SECRET, FleetGateway, discover_from_yaml
from router import RequestRouter

logger = logging.getLogger("fleet.cli")


# ---------------------------------------------------------------------------
# Subcommands
# ---------------------------------------------------------------------------

def cmd_serve(args: argparse.Namespace) -> None:
    """Start the fleet gateway server."""
    gw = FleetGateway(
        port=args.port,
        secret=args.secret,
        rate_limit=args.rate_limit,
        enable_auth=not args.no_auth,
        enable_rate_limit=not args.no_rate_limit,
        enable_cors=not args.no_cors,
    )
    print(f"🚀 Fleet Gateway starting on port {args.port}")
    print(f"   Auth: {'disabled' if args.no_auth else 'enabled'}")
    print(f"   Rate limit: {'disabled' if args.no_rate_limit else f'{args.rate_limit}/min'}")
    print(f"   CORS: {'disabled' if args.no_cors else 'enabled'}")
    print(f"   Fleet secret: {args.secret[:8]}...")
    print()
    gw.serve(blocking=True)


def cmd_routes(args: argparse.Namespace) -> None:
    """List all registered routes."""
    router = RequestRouter()
    # Auto-discover
    for agent in discover_from_yaml(args.fleet_yaml):
        router.register(
            name=agent["name"],
            host=agent["host"],
            port=agent["port"],
            friendly_names=agent.get("aliases"),
        )

    routes = router.list_routes()
    if not routes:
        print("No routes registered. Add agents to fleet.yaml or register via API.")
        return

    print(f"{'Pattern':<40} {'Agent':<20} {'Methods'}")
    print("-" * 80)
    for r in routes:
        methods = ", ".join(r["methods"])
        print(f"{r['pattern']:<40} {r['agent']:<20} {methods}")

    print(f"\nTotal: {len(routes)} route(s)")


def cmd_agents(args: argparse.Namespace) -> None:
    """List discovered fleet agents."""
    agents = discover_from_yaml(args.fleet_yaml)
    if not agents:
        print("No agents discovered. Create a fleet.yaml file.")
        return

    print(f"{'Name':<20} {'Host':<20} {'Port':<8} {'Aliases'}")
    print("-" * 70)
    for a in agents:
        aliases = ", ".join(a.get("aliases", []))
        print(f"{a['name']:<20} {a['host']:<20} {a['port']:<8} {aliases}")

    print(f"\nTotal: {len(agents)} agent(s)")


def cmd_test(args: argparse.Namespace) -> None:
    """Run connectivity test to all agents via the gateway."""
    base = f"http://localhost:{args.port}"
    errors = []

    # Test 1: Gateway health
    print("[1/4] Testing gateway health...")
    try:
        resp = _fetch(f"{base}/health")
        data = resp.json()
        status = data.get("status", "unknown")
        print(f"       Gateway status: {status}")
        if status == "healthy":
            print("       ✓ Gateway is healthy")
        else:
            print("       ⚠ Gateway is degraded")
    except Exception as e:
        errors.append(f"Health check failed: {e}")
        print(f"       ✗ {e}")

    # Test 2: Agent listing
    print("[2/4] Testing agent listing...")
    try:
        resp = _fetch(f"{base}/agents")
        data = resp.json()
        count = len(data)
        print(f"       Found {count} registered agent(s)")
        print("       ✓ Agent listing works")
    except Exception as e:
        errors.append(f"Agent listing failed: {e}")
        print(f"       ✗ {e}")

    # Test 3: Stats endpoint
    print("[3/4] Testing stats endpoint...")
    try:
        resp = _fetch(f"{base}/stats")
        data = resp.json()
        print(f"       Total requests: {data.get('total_requests', 0)}")
        print("       ✓ Stats endpoint works")
    except Exception as e:
        errors.append(f"Stats failed: {e}")
        print(f"       ✗ {e}")

    # Test 4: Registry
    print("[4/4] Testing registry...")
    try:
        resp = _fetch(f"{base}/registry")
        data = resp.json()
        print(f"       Registry has {len(data)} service(s)")
        print("       ✓ Registry works")
    except Exception as e:
        errors.append(f"Registry failed: {e}")
        print(f"       ✗ {e}")

    # Summary
    print()
    if errors:
        print(f"⚠ {len(errors)} test(s) failed:")
        for err in errors:
            print(f"  - {err}")
    else:
        print("✓ All connectivity tests passed!")


def cmd_onboard(args: argparse.Namespace) -> None:
    """Set up a new fleet gateway with a starter fleet.yaml."""
    import os

    yaml_path = args.fleet_yaml

    if os.path.isfile(yaml_path):
        print(f"fleet.yaml already exists at {yaml_path}")
        overwrite = input("Overwrite? [y/N] ").strip().lower()
        if overwrite != "y":
            print("Aborted.")
            return

    starter = """\
# Fleet Gateway Configuration
# This file defines the agents in your fleet.

agents:
  - name: trail-agent
    host: localhost
    port: 8501
    aliases: [trail, encoder]

  - name: git-agent
    host: localhost
    port: 8502
    aliases: [git, repo]

  - name: trust-agent
    host: localhost
    port: 8503
    aliases: [trust, verify]

  - name: keeper
    host: localhost
    port: 8504
    aliases: [keeper, state]
"""
    with open(yaml_path, "w") as f:
        f.write(starter)

    print(f"✓ Created {yaml_path} with starter configuration.")
    print()
    print("Next steps:")
    print("  1. Edit fleet.yaml to match your agent addresses")
    print("  2. Start your agents")
    print("  3. Run: python cli.py serve")
    print()
    print("Or register agents dynamically:")
    print(f"  curl -X POST http://localhost:{args.port}/registry/register \\")
    print(f'    -H "Authorization: Bearer fleet-secret-token" \\')
    print(f'    -H "Content-Type: application/json" \\')
    print(f'    -d \'{{"name":"my-agent","host":"localhost","port":9000}}\'')


# ---------------------------------------------------------------------------
# HTTP Helper
# ---------------------------------------------------------------------------

class _FetchResult:
    def __init__(self, status: int, body: bytes):
        self.status = status
        self._body = body

    def json(self):
        return json.loads(self._body)


def _fetch(url: str, timeout: float = 3.0) -> _FetchResult:
    try:
        req = urllib.request.Request(url)
        resp = urllib.request.urlopen(req, timeout=timeout)
        return _FetchResult(resp.status, resp.read())
    except urllib.error.HTTPError as e:
        body = e.read() if e.fp else b""
        return _FetchResult(e.code, body)


# ---------------------------------------------------------------------------
# Argument Parser
# ---------------------------------------------------------------------------

def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="fleet-gateway",
        description="Fleet API Gateway — unified HTTP interface to the Pelagic fleet",
    )
    parser.add_argument("--fleet-yaml", default="fleet.yaml", help="Path to fleet.yaml")
    subparsers = parser.add_subparsers(dest="command", help="Available commands")

    # serve
    serve_p = subparsers.add_parser("serve", help="Start the gateway server")
    serve_p.add_argument("--port", type=int, default=DEFAULT_PORT, help=f"Port (default: {DEFAULT_PORT})")
    serve_p.add_argument("--secret", default=DEFAULT_FLEET_SECRET, help="Fleet shared secret")
    serve_p.add_argument("--rate-limit", type=int, default=60, help="Rate limit (tokens/min)")
    serve_p.add_argument("--no-auth", action="store_true", help="Disable authentication")
    serve_p.add_argument("--no-rate-limit", action="store_true", help="Disable rate limiting")
    serve_p.add_argument("--no-cors", action="store_true", help="Disable CORS")

    # routes
    routes_p = subparsers.add_parser("routes", help="List registered routes")

    # agents
    agents_p = subparsers.add_parser("agents", help="List discovered agents")

    # test
    test_p = subparsers.add_parser("test", help="Run connectivity tests")
    test_p.add_argument("--port", type=int, default=DEFAULT_PORT, help=f"Gateway port (default: {DEFAULT_PORT})")

    # onboard
    onboard_p = subparsers.add_parser("onboard", help="Set up gateway with starter config")

    return parser


def main() -> None:
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s [%(name)s] %(levelname)s: %(message)s",
    )
    parser = build_parser()
    args = parser.parse_args()

    if args.command == "serve":
        cmd_serve(args)
    elif args.command == "routes":
        cmd_routes(args)
    elif args.command == "agents":
        cmd_agents(args)
    elif args.command == "test":
        cmd_test(args)
    elif args.command == "onboard":
        cmd_onboard(args)
    else:
        parser.print_help()
        sys.exit(1)


if __name__ == "__main__":
    main()
