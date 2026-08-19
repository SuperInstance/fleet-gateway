#!/usr/bin/env python3
"""fleet_gw — thin fail-open client shim for the Fleet Gateway.

Fleet Gateway (Rust) runs at http://127.0.0.1:8787 and speaks an
OpenAI-compatible proxy API. This shim's ONE job:

    1. Try the gateway first (2s connect timeout).
    2. If the gateway is DOWN (ConnectionError / Timeout), fall through to a
       DIRECT vendor call using keys from the environment.

Fail-open is non-negotiable: the gateway is an optimization, not a dependency.

stdlib-only (urllib). No external deps.

Usage:
    from fleet_gw import post, chat

    # Generic: any OpenAI-compatible path, provider used for direct fallback.
    resp = post("deepseek", "/v1/chat/completions", {...})

    # Convenience: model name drives provider detection, reply text returned.
    reply = chat("glm-5.3", [{"role": "user", "content": "Say OK"}])

Env keys honored for direct fallback (first found wins):
    ZAI_API_KEY | ZHIPUAI_API_KEY | BIGMODEL_API_KEY   -> api.z.ai
    DEEPSEEK_API_KEY                                   -> api.deepseek.com
    DEEPINFRA_API_KEY                                  -> api.deepinfra.com
    OLLAMA needs no key (localhost:11434)
"""

from __future__ import annotations

import json
import os
import socket
import urllib.error
import urllib.parse
import urllib.request

__all__ = ["post", "chat", "GATEWAY_URL"]

GATEWAY_URL = os.environ.get("FLEET_GATEWAY_URL", "http://127.0.0.1:8787")

# Connect timeout for the gateway probe. Plain urllib cannot separate
# connect from read timeouts, so we do an explicit TCP connect probe first
# (this budget), then give the actual request the full read budget below.
CONNECT_TIMEOUT = 2.0

# Overall budget for a direct vendor call (these are real remote APIs and may
# legitimately take tens of seconds to generate).
DIRECT_TIMEOUT = float(os.environ.get("FLEET_GW_DIRECT_TIMEOUT", "300"))

# ─── Direct vendor endpoints ────────────────────────────────────────────────
# provider -> (base_url, [env var names]) ; Ollama needs no key.
PROVIDERS = {
    "zai": (
        "https://api.z.ai/api/paas/v4",
        ["ZAI_API_KEY", "ZHIPUAI_API_KEY", "BIGMODEL_API_KEY"],
    ),
    "deepseek": ("https://api.deepseek.com/v1", ["DEEPSEEK_API_KEY"]),
    "deepinfra": ("https://api.deepinfra.com/v1/openai", ["DEEPINFRA_API_KEY"]),
    "ollama": ("http://localhost:11434/v1", []),
}

# Model-name heuristics -> provider (for chat()'s auto-detection).
_MODEL_HINTS = (
    ("deepseek", "deepseek"),
    ("glm", "zai"),
    ("granite", "ollama"),
    ("nomic", "ollama"),
)
# DeepInfra serves the long tail (vendor/model style names); it is the default
# for anything unrecognized.
_DEFAULT_PROVIDER = "deepinfra"


def _provider_for(model: str) -> str:
    m = model.lower()
    for prefix, provider in _MODEL_HINTS:
        if m.startswith(prefix):
            return provider
    return _DEFAULT_PROVIDER


def _http_json(url: str, payload: dict, api_key: str | None, timeout: float) -> dict:
    """POST payload as JSON, return parsed JSON body. Raises on HTTP errors."""
    body = json.dumps(payload).encode("utf-8")
    req = urllib.request.Request(url, data=body, method="POST")
    req.add_header("Content-Type", "application/json")
    if api_key:
        req.add_header("Authorization", f"Bearer {api_key}")
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read().decode("utf-8"))


def _is_gateway_down(exc: BaseException) -> bool:
    """True only for connection-level failures (gateway unreachable)."""
    if isinstance(exc, (ConnectionError, socket.timeout, TimeoutError)):
        return True
    if isinstance(exc, urllib.error.URLError):
        reason = exc.reason
        if isinstance(reason, (ConnectionError, socket.timeout, TimeoutError)):
            return True
        # "connection refused" / "timed out" arrive as plain OSError strings
        msg = str(reason).lower()
        return "refused" in msg or "timed out" in msg or "unreachable" in msg
    return False


def _gateway_reachable() -> bool:
    """Cheap TCP connect probe within CONNECT_TIMEOUT seconds."""
    try:
        parts = urllib.parse.urlsplit(GATEWAY_URL)
        host = parts.hostname or "127.0.0.1"
        port = parts.port or (443 if parts.scheme == "https" else 80)
        with socket.create_connection((host, port), timeout=CONNECT_TIMEOUT):
            pass
        return True
    except OSError:
        return False


def post(provider: str, path: str, payload: dict) -> dict:
    """POST an OpenAI-compatible payload, gateway-first with fail-open.

    provider: vendor name for the DIRECT fallback if the gateway is down
              ("zai" | "deepseek" | "deepinfra" | "ollama").
    path:     OpenAI-style path, e.g. "/v1/chat/completions".
    payload:  JSON body dict, e.g. {"model": ..., "messages": [...]}.

    Returns the parsed JSON response (dict).
    Raises RuntimeError if the gateway is down AND no direct fallback is
    possible (unknown provider, missing env key, vendor also unreachable).
    """
    # 1. Gateway first: 2s connect probe, then a generous read budget so slow
    #    generations are never mistaken for a dead gateway.
    if _gateway_reachable():
        try:
            return _http_json(GATEWAY_URL + path, payload, None, DIRECT_TIMEOUT)
        except Exception as exc:
            if not _is_gateway_down(exc):
                raise  # gateway answered (even with an HTTP error) — pass it up

    # 2. Fail open: direct vendor call.
    if provider not in PROVIDERS:
        raise RuntimeError(
            f"fleet-gateway down and unknown provider {provider!r}; "
            f"expected one of {sorted(PROVIDERS)}"
        )
    base_url, key_envs = PROVIDERS[provider]
    api_key = next((os.environ[k] for k in key_envs if os.environ.get(k)), None)
    if key_envs and not api_key:
        raise RuntimeError(
            f"fleet-gateway down; direct {provider} fallback needs one of "
            f"{key_envs} in the environment"
        )
    # Provider base URLs already carry their version prefix (/v1, /v1/openai,
    # /api/paas/v4), so strip the OpenAI-style "/v1" from the client path —
    # exactly what the gateway itself does when routing upstream.
    sub = path.lstrip("/")
    if sub.startswith("v1/"):
        sub = sub[len("v1/"):]
    try:
        return _http_json(base_url.rstrip("/") + "/" + sub, payload, api_key, DIRECT_TIMEOUT)
    except Exception as exc:
        raise RuntimeError(
            f"fleet-gateway down and direct {provider} call failed: {exc}"
        ) from exc


def chat(model: str, messages: list, **kwargs) -> str:
    """Convenience: one chat completion, returns the assistant reply text.

    model, messages: standard OpenAI fields.
    **kwargs: forwarded (max_tokens, temperature, stream=False, ...).
    Provider is inferred from the model name for the fail-open path.
    """
    payload = {"model": model, "messages": messages, **kwargs}
    resp = post(_provider_for(model), "/v1/chat/completions", payload)
    try:
        msg = resp["choices"][0]["message"]
        content = msg.get("content")
        # Reasoning models (e.g. glm-5.3) can exhaust max_tokens on hidden
        # reasoning; surface that rather than an empty string.
        if not content and msg.get("reasoning_content"):
            return msg["reasoning_content"]
        if isinstance(content, str):
            return content
    except (KeyError, IndexError, TypeError):
        pass
    # Non-standard shape (e.g. an error body) — hand back the raw dict.
    return resp


if __name__ == "__main__":
    import sys

    reply = chat(
        sys.argv[1] if len(sys.argv) > 1 else "glm-5.3",
        [{"role": "user", "content": " ".join(sys.argv[2:]) or "Say OK"}],
        max_tokens=10,
    )
    print(reply)
