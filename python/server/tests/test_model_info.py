"""Best-effort model output-token ceiling from the gateway's ``/model/info``.

Mirrors the Rust server's ``map_model_info`` / ``model_output_ceiling`` tests in
``admin.rs``. The ceiling is what the turn runner threads into the engine's clamp
(EPIC th-1cc9fa).
"""

from __future__ import annotations

from typing import Any

import pytest

from smooth_operator_server import model_info


@pytest.fixture(autouse=True)
def _clear_cache():
    model_info.reset_cache()
    yield
    model_info.reset_cache()


# ── map_model_info (pure) ─────────────────────────────────────────────────────

SAMPLE_PAYLOAD: dict[str, Any] = {
    "data": [
        {"model_name": "claude-haiku-4-5", "model_info": {"max_output_tokens": 8192, "input_cost_per_token": 1e-6}},
        {"model_name": "big-context", "model_info": {"max_output_tokens": 65536}},
        {"model_name": "no-ceiling", "model_info": {"input_cost_per_token": 2e-6}},  # no max_output_tokens
    ]
}


def test_map_extracts_positive_ceilings():
    out = model_info.map_model_info(SAMPLE_PAYLOAD)
    assert out == {"claude-haiku-4-5": 8192, "big-context": 65536}


def test_map_skips_missing_name_and_nonpositive_and_nonint():
    payload = {
        "data": [
            {"model_info": {"max_output_tokens": 100}},  # no model_name → skip
            {"model_name": "zero", "model_info": {"max_output_tokens": 0}},  # 0 → skip
            {"model_name": "neg", "model_info": {"max_output_tokens": -5}},  # negative → skip
            {"model_name": "boolish", "model_info": {"max_output_tokens": True}},  # bool → skip
            {"model_name": "floaty", "model_info": {"max_output_tokens": 1.5}},  # float → skip
            {"model_name": "bare", "model_info": {}},  # no field → skip
            {"model_name": "noinfo"},  # no model_info → skip
        ]
    }
    assert model_info.map_model_info(payload) == {}


def test_map_tolerates_garbage_payloads():
    assert model_info.map_model_info({}) == {}
    assert model_info.map_model_info({"data": "nope"}) == {}
    assert model_info.map_model_info({"data": [None, 42, "x"]}) == {}
    assert model_info.map_model_info(None) == {}
    assert model_info.map_model_info([]) == {}


# ── model_output_ceiling (async, cached, best-effort) ─────────────────────────


async def _fetch_sample(url: str, key: str | None) -> dict[str, Any]:
    return SAMPLE_PAYLOAD


@pytest.mark.asyncio
async def test_ceiling_looked_up_for_known_model():
    ceiling = await model_info.model_output_ceiling("big-context", gateway_key="sk-x", fetch=_fetch_sample)
    assert ceiling == 65536


@pytest.mark.asyncio
async def test_ceiling_none_for_unknown_model():
    ceiling = await model_info.model_output_ceiling("who-dis", gateway_key="sk-x", fetch=_fetch_sample)
    assert ceiling is None


@pytest.mark.asyncio
async def test_no_key_skips_fetch_and_returns_none():
    called = False

    async def _boom(url: str, key: str | None) -> dict[str, Any]:
        nonlocal called
        called = True
        raise AssertionError("must not fetch without a key")

    ceiling = await model_info.model_output_ceiling("big-context", gateway_key="", fetch=_boom)
    assert ceiling is None
    assert called is False


@pytest.mark.asyncio
async def test_fetch_error_returns_none_and_does_not_cache():
    attempts = 0

    async def _flaky(url: str, key: str | None) -> dict[str, Any]:
        nonlocal attempts
        attempts += 1
        raise RuntimeError("gateway down")

    assert await model_info.model_output_ceiling("big-context", gateway_key="sk-x", fetch=_flaky) is None
    # A failed fetch is NOT cached → the next call retries (mirrors the Rust behaviour).
    assert await model_info.model_output_ceiling("big-context", gateway_key="sk-x", fetch=_flaky) is None
    assert attempts == 2


@pytest.mark.asyncio
async def test_success_is_cached_fetch_runs_once():
    calls = 0

    async def _count(url: str, key: str | None) -> dict[str, Any]:
        nonlocal calls
        calls += 1
        return SAMPLE_PAYLOAD

    a = await model_info.model_output_ceiling("claude-haiku-4-5", gateway_key="sk-x", fetch=_count)
    b = await model_info.model_output_ceiling("big-context", gateway_key="sk-x", fetch=_count)
    assert a == 8192
    assert b == 65536
    assert calls == 1  # cached across models after the first success


@pytest.mark.asyncio
async def test_ceiling_url_uses_model_info_endpoint():
    seen: dict[str, Any] = {}

    async def _capture(url: str, key: str | None) -> dict[str, Any]:
        seen["url"] = url
        seen["key"] = key
        return SAMPLE_PAYLOAD

    await model_info.model_output_ceiling(
        "big-context", gateway_url="https://llm.smoo.ai/v1/", gateway_key="sk-x", fetch=_capture
    )
    # Trailing slash trimmed; `/model/info` appended.
    assert seen["url"] == "https://llm.smoo.ai/v1/model/info"
    assert seen["key"] == "sk-x"
