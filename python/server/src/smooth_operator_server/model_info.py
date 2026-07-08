"""Best-effort per-model **output-token ceiling** from the gateway's ``/model/info``.

A budget/policy ``max_tokens`` can exceed what a model can physically emit — a
reasoning model then burns the whole budget on reasoning and returns EMPTY, or the
upstream 400s (e.g. ``groq-compound`` caps output at 8192). The actual clamp lives
in the engine (``smooth_operator_core.effective_max_tokens``); this module *sources*
the ceiling the turn runner threads into ``AgentOptions.model_max_output``.

Mirrors the Rust server's ``admin.rs`` ``map_model_info`` / ``model_output_ceiling``
(EPIC th-1cc9fa). Fetched at most once per process (module cache), reusing the same
gateway creds the turns use. **Best-effort**: no gateway key, any transport error,
an unknown model, or a model whose gateway entry has no positive ceiling ⇒ ``None``
⇒ the engine leaves ``max_tokens`` unclamped (graceful passthrough, no behaviour
change).

Zero extra runtime deps: the fetch uses ``urllib`` from the stdlib in a worker
thread. ``ponytail:`` stdlib GET is enough — the only consumer is one integer per
model; no need to pull in httpx just for this.
"""

from __future__ import annotations

import asyncio
import json
import os
import urllib.request
from typing import Any, Awaitable, Callable

#: Default OpenAI-compatible gateway (matches the Rust server's ``DEFAULT_GATEWAY_URL``).
DEFAULT_GATEWAY_URL = "https://llm.smoo.ai/v1"

#: A seam for tests: given ``(url, key)`` return the parsed ``/model/info`` JSON, or
#: raise. Production uses :func:`_default_fetch`; tests inject a stub so the ceiling
#: path is exercised with no network.
Fetcher = Callable[[str, str | None], Awaitable[dict[str, Any]]]

#: Process-wide cache of ``{model_name: max_output_tokens}``. ``None`` until the first
#: successful fetch — a failed/keyless attempt is NOT cached, so the next turn retries
#: (mirrors the Rust ``model_output_ceiling`` "cache success only" behaviour).
_cache: dict[str, int] | None = None


def map_model_info(payload: Any) -> dict[str, int]:
    """Map a gateway ``/model/info`` payload
    (``{ data: [{ model_name, model_info: { max_output_tokens, ... } }] }``) to
    ``{model_name: max_output_tokens}``, keeping ONLY models that report a positive
    integer ceiling. Entries without a name or a usable ceiling are skipped. Pure +
    network-free so it's unit-testable on a sample payload.

    Unlike the Rust ``map_model_info`` (which also surfaces cost/tier/useCases for the
    ``/admin/model-costs`` badge), the Python server has no model-costs route — the
    only consumer is the ceiling clamp, so this maps just the ceiling. ``ponytail:``
    map what's read; add cost/tier here if a ``/admin/model-costs`` route ever lands."""
    out: dict[str, int] = {}
    data = payload.get("data") if isinstance(payload, dict) else None
    if not isinstance(data, list):
        return out
    for entry in data:
        if not isinstance(entry, dict):
            continue
        name = entry.get("model_name")
        if not isinstance(name, str) or not name:
            continue
        info = entry.get("model_info")
        raw = info.get("max_output_tokens") if isinstance(info, dict) else None
        # bool is an int subclass — reject True/False so a stray boolean ceiling
        # never sneaks in as 1/0.
        if isinstance(raw, bool) or not isinstance(raw, int):
            continue
        if raw > 0:
            out[name] = raw
    return out


def _default_fetch(url: str, key: str | None) -> dict[str, Any]:
    """Blocking ``GET {url}`` with an optional bearer, parsed as JSON. Run via
    :func:`asyncio.to_thread` so it never blocks the event loop."""
    req = urllib.request.Request(url)  # noqa: S310 — fixed https gateway URL, not user input
    if key:
        req.add_header("Authorization", f"Bearer {key}")
    with urllib.request.urlopen(req, timeout=10) as resp:  # noqa: S310
        return json.loads(resp.read().decode("utf-8"))


async def model_output_ceiling(
    model: str,
    *,
    gateway_url: str | None = None,
    gateway_key: str | None = None,
    fetch: Fetcher | None = None,
) -> int | None:
    """The ``model``'s hard output ceiling (``max_output_tokens``) from the gateway,
    or ``None`` when unknown. Threaded into ``AgentOptions.model_max_output`` so the
    engine clamps ``max_tokens`` to what the model can emit (EPIC th-1cc9fa).

    Gateway URL/key default to ``SMOOAI_GATEWAY_URL`` / ``SMOOAI_GATEWAY_KEY`` (the
    same creds :func:`smooth_operator_server.server._build_gateway_client` uses).
    **No key ⇒ ``None`` with no network call** — a keyless server runs on a mock
    client (tests) and never needs a live ceiling. Any fetch error ⇒ ``None`` and the
    failure is not cached (next turn retries)."""
    global _cache
    if _cache is None:
        key = gateway_key if gateway_key is not None else os.environ.get("SMOOAI_GATEWAY_KEY")
        if not key:
            return None  # keyless → nothing to clamp against; skip the fetch entirely
        base = gateway_url or os.environ.get("SMOOAI_GATEWAY_URL") or DEFAULT_GATEWAY_URL
        url = f"{base.rstrip('/')}/model/info"
        fetcher = fetch or (lambda u, k: asyncio.to_thread(_default_fetch, u, k))
        try:
            payload = await fetcher(url, key)
        except Exception:
            return None  # best-effort: gateway/transport/decode error ⇒ unclamped
        _cache = map_model_info(payload)
    ceiling = _cache.get(model)
    return ceiling if isinstance(ceiling, int) and ceiling > 0 else None


def reset_cache() -> None:
    """Drop the process-wide ceiling cache. For tests that exercise multiple fetch
    outcomes; production fetches once and keeps it."""
    global _cache
    _cache = None
