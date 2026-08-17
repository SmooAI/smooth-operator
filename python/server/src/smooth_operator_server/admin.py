"""The ``/admin/*`` management API — what the console (``console/``) drives.

Wire contract is the Rust server's ``rust/smooth-operator-server/src/admin.rs``:
same paths, same **camelCase** JSON, the same ``{"error":{"code","message"}}``
envelope, and the same role gate (Bearer token → verify → rank check; 401
missing/invalid, 403 insufficient). Rank: basic=0, curator=1, admin=2.

Shapes are built against ``console/lib/types.ts``, not Rust's field names: Rust's
structs read snake_case in source but carry ``#[serde(rename_all = "camelCase")]``,
so copying the field names would produce a server that passes its own tests and
renders nothing.

**Why this listens on its own port.** Rust, Go, C# and TypeScript serve `/admin/*`
and `/ws` on ONE port. This server speaks WebSocket via ``websockets``, whose
handshake parser accepts GET only and raises ``ValueError("unsupported request
body")`` on any non-zero ``Content-Length`` — the request never reaches
``process_request``. Half this API is POST/PUT with a JSON body, so that hook
structurally cannot serve it. Instead the admin API runs on a small stdlib
``http.server`` listener alongside the WebSocket one (default: ws port + 1,
override with ``admin_port``). The console points at it via its own admin base
URL, which is already configured separately from the WS URL.

ponytail: connector configs, settings and indexing runs live in memory
(:class:`AdminStores`) because this server is memory-only today. The durable
storage adapter is a separate workstream — swap those three containers; nothing
outside this module reads them.
"""

from __future__ import annotations

import asyncio
import json
import re
import threading
import uuid
from dataclasses import dataclass, field
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any, Optional
from urllib.parse import parse_qs, urlsplit

from .auth import AccessContext, Principal

# Role ranks, mirroring Rust's ``role_rank``.
ROLE_BASIC = 0
ROLE_CURATOR = 1
ROLE_ADMIN = 2


def _role_rank(role: str) -> int:
    """Unknown/empty roles are basic — fail closed on privilege, not open."""
    match role.strip().lower():
        case "admin":
            return ROLE_ADMIN
        case "curator":
            return ROLE_CURATOR
        case _:
            return ROLE_BASIC


def _rank_name(rank: int) -> str:
    return "admin" if rank == ROLE_ADMIN else "curator" if rank == ROLE_CURATOR else "basic"


def _now() -> str:
    return datetime.now(timezone.utc).isoformat()


@dataclass
class AdminStores:
    """Org-scoped admin state. Every read and write filters by org, so one org can
    never see or mutate another's rows."""

    connectors: dict[str, dict[str, Any]] = field(default_factory=dict)
    settings: dict[str, dict[str, Any]] = field(default_factory=dict)
    runs: list[dict[str, Any]] = field(default_factory=list)


def _default_settings(org_id: str) -> dict[str, Any]:
    """Rust's "defaults when unset" settings read."""
    return {
        "orgId": org_id,
        "model": "claude-haiku-4-5",
        "systemPrompt": "",
        "defaultTools": [],
        "updatedAt": _now(),
    }


def _public(row: dict[str, Any]) -> dict[str, Any]:
    """Strip the internal owner key before serializing."""
    return {k: v for k, v in row.items() if k != "_orgId"}


# ── responses ───────────────────────────────────────────────────────────────


#: One admin response: an HTTP status and a JSON body (``None`` for 204).
AdminResponse = tuple[int, Optional[dict[str, Any]]]


def _json(status: int, body: Any) -> AdminResponse:
    return status, body


def _error(status: int, code: str, message: str) -> AdminResponse:
    return status, {"error": {"code": code, "message": message}}


# ── auth gate ───────────────────────────────────────────────────────────────


def _bearer_token(headers: Any) -> Optional[str]:
    """The raw token from ``Authorization: Bearer <token>``, or ``None``."""
    value = headers.get("Authorization") if headers is not None else None
    if not value or not value.lower().startswith("bearer "):
        return None
    token = value[len("bearer ") :].strip()
    return token or None


@dataclass
class _Denied:
    """A rejection to return instead of a principal."""

    response: AdminResponse


def _require_role(state: Any, headers: Any, min_rank: int) -> Principal | _Denied:
    """Authenticate and enforce a minimum role. Fails CLOSED: no token is 401 even
    on a no-auth server."""
    token = _bearer_token(headers)
    if token is None:
        return _Denied(_error(401, "UNAUTHENTICATED", "missing bearer token"))

    access: AccessContext = state.auth.resolve(token)
    # NOTE the polarity: this server's AccessContext carries ``auth_disabled``
    # (True == no auth configured), the inverse of the Go/TS ``authEnabled``. An
    # auth-ENABLED server that could not verify the token yields an anonymous
    # context, which must never satisfy an admin route.
    if not access.auth_disabled and access.is_anonymous:
        return _Denied(_error(401, "INVALID_TOKEN", "invalid bearer token"))

    principal = access.principal
    role = principal.role
    # AUTH_MODE=none (dev) grants Admin, exactly as Rust's NoAuthVerifier does —
    # otherwise the console 403-walls against a local server, which is as useless
    # as the 404s this API exists to fix. Only the explicit dev verifier takes this
    # path; an auth-enabled server is unaffected.
    if state.auth.mode() == "none":
        role = "admin"

    if _role_rank(role) < min_rank:
        return _Denied(
            _error(
                403,
                "FORBIDDEN",
                f"requires role {_rank_name(min_rank)}, principal has {_rank_name(_role_rank(role))}",
            )
        )
    # Hand back a principal whose role reflects the grant, so /admin/me is honest.
    return Principal(
        sub=principal.sub, org=principal.org, role=role, groups=list(principal.groups), email=principal.email
    )


# ── the handler ─────────────────────────────────────────────────────────────

_MESSAGES_RE = re.compile(r"^/admin/conversations/([^/]+)/messages$")
_INDEX_RE = re.compile(r"^/admin/connectors/([^/]+)/index$")
_CONNECTOR_RE = re.compile(r"^/admin/connectors/([^/]+)$")


async def handle_admin_request(
    state: Any, method: str, target: str, headers: Any, body_bytes: bytes
) -> Optional[AdminResponse]:
    """Serve one ``/admin/*`` request. ``None`` when the path is not an admin route,
    so the caller can answer it however it likes."""
    split = urlsplit(target)
    path = split.path
    if not path.startswith("/admin/"):
        return None

    try:
        return await _route(state, method.upper(), path, split.query, headers, body_bytes)
    except json.JSONDecodeError:
        return _error(400, "INVALID_BODY", "malformed JSON body")
    except Exception:  # noqa: BLE001 - never leak an internal error to the console
        return _error(500, "INTERNAL", "admin request failed")


async def _route(state: Any, method: str, path: str, query: str, headers: Any, body_bytes: bytes) -> AdminResponse:
    params = parse_qs(query)

    def body() -> dict[str, Any]:
        raw = body_bytes.decode("utf-8").strip()
        return json.loads(raw) if raw else {}

    # Ungated, exactly as in Rust: the console probes health before it has a token.
    if method == "GET" and path == "/admin/health":
        return _json(200, {"status": "ok"})

    if method == "GET" and path == "/admin/me":
        p = _require_role(state, headers, ROLE_BASIC)
        if isinstance(p, _Denied):
            return p.response
        return _json(200, {"userId": p.sub, "orgId": p.org, "role": _rank_name(_role_rank(p.role))})

    if method == "GET" and path == "/admin/conversations":
        p = _require_role(state, headers, ROLE_BASIC)
        if isinstance(p, _Denied):
            return p.response
        limit = int(params.get("limit", ["50"])[0] or 50)
        cursor = int(params.get("cursor", ["0"])[0] or 0)
        summaries = sorted(await state.store.list_conversations(p.email), key=lambda c: c.updated_at, reverse=True)
        page = summaries[cursor : cursor + limit]
        end = cursor + len(page)
        return _json(
            200,
            {
                "conversations": [
                    {
                        "id": c.conversation_id,
                        "name": getattr(c, "first_inbound_text", "") or "Conversation",
                        "platform": "web",
                        "createdAt": c.updated_at,
                        "updatedAt": c.updated_at,
                    }
                    for c in page
                ],
                "nextCursor": end if end < len(summaries) else None,
            },
        )

    if method == "GET" and (m := _MESSAGES_RE.match(path)):
        denied = _require_role(state, headers, ROLE_BASIC)
        if isinstance(denied, _Denied):
            return denied.response
        conversation_id = m.group(1)
        stored = await state.store.list_messages(conversation_id, 500)
        return _json(
            200,
            {
                "conversationId": conversation_id,
                "messages": [
                    {
                        "id": msg.id,
                        "conversationId": msg.conversation_id,
                        "direction": msg.direction,
                        "content": {"items": [{"type": "text", "text": msg.text}], "text": msg.text},
                        "createdAt": msg.created_at,
                    }
                    for msg in stored
                ],
                "nextCursor": None,
            },
        )

    if method == "GET" and path == "/admin/indexing/runs":
        p = _require_role(state, headers, ROLE_CURATOR)
        if isinstance(p, _Denied):
            return p.response
        return _json(200, {"runs": [_public(r) for r in state.admin.runs if r["_orgId"] == p.org]})

    if method == "GET" and path == "/admin/document-sets":
        denied = _require_role(state, headers, ROLE_CURATOR)
        if isinstance(denied, _Denied):
            return denied.response
        # ponytail: no knowledge store on this server yet, so there are no document
        # sets to count. An empty list is the honest answer and renders fine.
        return _json(200, {"documentSets": []})

    if path == "/admin/connectors":
        if method == "GET":
            p = _require_role(state, headers, ROLE_CURATOR)
            if isinstance(p, _Denied):
                return p.response
            rows = sorted(
                (c for c in state.admin.connectors.values() if c["_orgId"] == p.org),
                key=lambda c: c["name"],
            )
            return _json(200, {"connectors": [_public(c) for c in rows]})
        if method == "POST":
            p = _require_role(state, headers, ROLE_ADMIN)
            if isinstance(p, _Denied):
                return p.response
            write = _validate_connector(body())
            if isinstance(write, tuple):
                return write
            now = _now()
            row = {"id": str(uuid.uuid4()), **write, "createdAt": now, "updatedAt": now, "_orgId": p.org}
            state.admin.connectors[row["id"]] = row
            return _json(200, {"connector": _public(row)})

    if method == "POST" and (m := _INDEX_RE.match(path)):
        p = _require_role(state, headers, ROLE_CURATOR)
        if isinstance(p, _Denied):
            return p.response
        row = _owned_connector(state, m.group(1), p.org)
        if row is None:
            return _error(404, "NOT_FOUND", "connector not found")
        # ponytail: no ingestion pipeline on this server yet, so the run is recorded
        # as succeeded with zero documents rather than faked with invented counts.
        now = _now()
        run = {
            "id": str(uuid.uuid4()),
            "connectorName": row["name"],
            "status": "succeeded",
            "startedAt": now,
            "finishedAt": now,
            "documentsSeen": 0,
            "chunksIndexed": 0,
            "documentsSkipped": 0,
            "error": None,
            "_orgId": p.org,
        }
        state.admin.runs.append(run)
        return _json(200, {"run": _public(run)})

    if m := _CONNECTOR_RE.match(path):
        connector_id = m.group(1)
        if method == "GET":
            p = _require_role(state, headers, ROLE_CURATOR)
            if isinstance(p, _Denied):
                return p.response
            row = _owned_connector(state, connector_id, p.org)
            return _json(200, {"connector": _public(row)}) if row else _error(404, "NOT_FOUND", "connector not found")
        if method == "PUT":
            p = _require_role(state, headers, ROLE_ADMIN)
            if isinstance(p, _Denied):
                return p.response
            write = _validate_connector(body())
            if isinstance(write, tuple):
                return write
            row = _owned_connector(state, connector_id, p.org)
            if row is None:
                return _error(404, "NOT_FOUND", "connector not found")
            row.update(write, updatedAt=_now())
            return _json(200, {"connector": _public(row)})
        if method == "DELETE":
            p = _require_role(state, headers, ROLE_ADMIN)
            if isinstance(p, _Denied):
                return p.response
            if _owned_connector(state, connector_id, p.org) is None:
                return _error(404, "NOT_FOUND", "connector not found")
            del state.admin.connectors[connector_id]
            return 204, None

    if path == "/admin/settings":
        if method == "GET":
            p = _require_role(state, headers, ROLE_CURATOR)
            if isinstance(p, _Denied):
                return p.response
            return _json(200, {"settings": state.admin.settings.get(p.org) or _default_settings(p.org)})
        if method == "PUT":
            p = _require_role(state, headers, ROLE_ADMIN)
            if isinstance(p, _Denied):
                return p.response
            data = body()
            model = data.get("model")
            if not isinstance(model, str) or not model.strip():
                return _error(400, "INVALID_BODY", "model is required")
            settings = {
                "orgId": p.org,
                "model": model,
                "systemPrompt": data.get("systemPrompt") if isinstance(data.get("systemPrompt"), str) else "",
                "defaultTools": data.get("defaultTools") if isinstance(data.get("defaultTools"), list) else [],
                "updatedAt": _now(),
            }
            state.admin.settings[p.org] = settings
            return _json(200, {"settings": settings})

    return _error(404, "NOT_FOUND", f"no admin route for {method} {path}")


def _owned_connector(state: Any, connector_id: str, org_id: str) -> Optional[dict[str, Any]]:
    """An org-owned connector, or ``None``. A cross-org id is deliberately
    indistinguishable from an unknown one — both 404."""
    row = state.admin.connectors.get(connector_id)
    return row if row is not None and row["_orgId"] == org_id else None


def _validate_connector(data: dict[str, Any]) -> dict[str, Any] | AdminResponse:
    """Validate a connector write body, or a 400."""
    name, kind = data.get("name"), data.get("kind")
    if not isinstance(name, str) or not name.strip() or not isinstance(kind, str) or not kind.strip():
        return _error(400, "INVALID_BODY", "name and kind are required")
    config = data.get("config")
    return {
        "name": name,
        "kind": kind,
        "config": config if isinstance(config, dict) else {},
        "enabled": data.get("enabled") is True,
    }


# ── the listener ────────────────────────────────────────────────────────────


class _AdminHTTPRequestHandler(BaseHTTPRequestHandler):
    """Bridges one stdlib HTTP request onto :func:`handle_admin_request`.

    ``server.admin_state`` and ``server.admin_loop`` are set by
    :func:`start_admin_http_server`. The routing coroutine is submitted to the
    server's event loop, so handlers see the same in-memory state the WebSocket
    side does without any locking of their own.
    """

    protocol_version = "HTTP/1.1"

    def _serve(self) -> None:
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else b""
        state = self.server.admin_state  # type: ignore[attr-defined]
        loop = self.server.admin_loop  # type: ignore[attr-defined]

        future = asyncio.run_coroutine_threadsafe(
            handle_admin_request(state, self.command, self.path, self.headers, body), loop
        )
        result = future.result(timeout=30)

        if result is None:
            status, payload = 404, {"error": {"code": "NOT_FOUND", "message": "not an admin route"}}
        else:
            status, payload = result

        raw = b"" if payload is None else json.dumps(payload).encode("utf-8")
        self.send_response(status)
        if raw:
            self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        if raw:
            self.wfile.write(raw)

    do_GET = do_POST = do_PUT = do_DELETE = _serve

    def log_message(self, *args: Any) -> None:
        """Silence the default stderr access log."""


def start_admin_http_server(state: Any, host: str, port: int) -> ThreadingHTTPServer:
    """Start the `/admin/*` listener on its own thread. Caller owns shutdown."""
    httpd = ThreadingHTTPServer((host, port), _AdminHTTPRequestHandler)
    httpd.daemon_threads = True
    httpd.admin_state = state  # type: ignore[attr-defined]
    httpd.admin_loop = asyncio.get_running_loop()  # type: ignore[attr-defined]
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    return httpd
