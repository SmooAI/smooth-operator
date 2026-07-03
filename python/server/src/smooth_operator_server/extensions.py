"""SEP extension hosting for the operator server.

Wires the engine's :class:`~smooth_operator_core.extension.ExtensionHost` into a
turn so a server-side agent can host extensions: discover ``extension.toml``
extensions, spawn them as JSON-RPC/ndjson subprocesses, and register their tools
into the turn's tool set (flowing through the same per-agent ``enabled_tools``
filtering the runner applies). The Python sibling of the Rust reference
``rust/smooth-operator-server/src/extensions.rs`` (smooth-operator#159).

## Trust — default deny
The server has no interactive trust prompt (a multi-session daemon can't stop to
ask a human). ``SMOOTH_EXTENSIONS_ALLOW`` (comma-separated extension names) IS the
trust decision: empty (the default) => **no extension is ever spawned** and the
host is never built, so behavior is byte-for-byte unchanged.

## ``ui/confirm`` -> the existing confirmation frame
:class:`ConfirmUiProvider` projects an extension's ``ui/confirm`` onto the operator
protocol's ``write_confirmation_required`` / ``confirm_tool_action`` frames — the
same out-of-band bridge the native write-tool HITL uses (the session-keyed
:class:`~smooth_operator_server.confirmation.ConfirmationRegistry`): register a
resumable future under the session, emit the frame, and park the extension's request
until the client answers with ``confirm_tool_action``. Every other ``ui/*`` degrades
headless (interactive -> ``{cancelled}``, render-only -> ``{}``); we advertise only
the ``confirm`` capability at handshake so a well-behaved extension gates the rest
off via ``hasUI``.
"""

from __future__ import annotations

import asyncio
import logging
import os
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from smooth_operator_core.extension import (
    DiscoveredExtension,
    ExtensionHost,
    HostDelegate,
    HostInfo,
    WorkspaceInfo,
    discover,
)
from smooth_operator_core.extension.manifest import default_global_dir, project_dir

from . import protocol
from .confirmation import ConfirmationRegistry

_log = logging.getLogger("smooth_operator_server.extensions")

#: Frontend ``mode`` announced to extensions at handshake. The servers front the
#: chat-widget, whose confirm/select land as chat-native button frames.
UI_MODE = "widget"

#: How long a parked ``ui/confirm`` waits for the client's ``confirm_tool_action``
#: before the bridge resolves it as cancelled. Matches the native write-tool
#: confirmation window so both HITL paths behave the same to a client.
UI_CONFIRM_TIMEOUT = 300.0


def parse_allowlist(raw: str | None) -> list[str]:
    """Parse ``SMOOTH_EXTENSIONS_ALLOW`` into a list of allowed extension names
    (comma-separated, trimmed, empties dropped). Absent/blank => empty => deny all."""
    if not raw:
        return []
    return [name for name in (part.strip() for part in raw.split(",")) if name]


@dataclass
class ExtensionTurn:
    """A per-turn extension host plus the session it belongs to. The runner registers
    the host's tools, adds them to the agent's tool set, and calls :meth:`teardown` at
    turn end to stop the subprocesses and drop any parked confirmation."""

    host: ExtensionHost
    session_id: str
    confirmations: ConfirmationRegistry

    async def teardown(self) -> None:
        # Clear any ui/confirm responder still parked for the session, then shut down
        # every extension subprocess (5s grace each). Mirrors the Rust `(clear)` +
        # host drop at turn end.
        self.confirmations.clear(self.session_id)
        await self.host.shutdown_all()


class ConfirmUiProvider(HostDelegate):
    """The :class:`HostDelegate` that bridges ``ui/confirm`` onto the confirmation
    frame and degrades every other ``ui/*`` headless. Bound to ONE turn (its sink,
    request id, session), which is why the host is built per turn — a shared host
    could not route a ``ui/*`` back to the right session's socket."""

    def __init__(
        self,
        sink: Any,
        request_id: str,
        session_id: str,
        confirmations: ConfirmationRegistry,
    ) -> None:
        self._sink = sink
        self._request_id = request_id
        self._session_id = session_id
        self._confirmations = confirmations

    async def ui_request(self, ext: str, params: Any) -> Any:
        kind = params.get("kind") if isinstance(params, dict) else None
        if kind == "confirm":
            prompt = (params.get("prompt") if isinstance(params, dict) else None) or "Confirm this action?"
            # Register a fresh responder for this session so the next inbound
            # `confirm_tool_action` resumes THIS request, then emit the frame and park
            # until the human answers (or we time out).
            future = self._confirmations.register(self._session_id)
            self._sink(protocol.write_confirmation_required(self._request_id, ext, prompt))
            try:
                approved = await asyncio.wait_for(future, UI_CONFIRM_TIMEOUT)
            except (asyncio.TimeoutError, asyncio.CancelledError):
                # Our own timeout / turn teardown reads as a dismissed dialog.
                return {"cancelled": True}
            return {"confirmed": True} if approved else {"confirmed": False}
        # Render-only kinds: accept and drop — there's no chat frame for them.
        if kind in ("notify", "set_status", "set_widget", "set_title"):
            return {}
        # select/input need an answer we can't source from a confirm button.
        return {"cancelled": True}


async def build_extension_host(
    session_id: str,
    request_id: str,
    sink: Any,
    confirmations: ConfirmationRegistry,
) -> ExtensionTurn | None:
    """Discover, trust-gate (allowlist), and load the per-turn extension host for a
    session's turn. Returns ``None`` — the host is never built, zero overhead — when
    the allowlist is empty (default deny) or no allowed extension loads."""
    # Trust = a default-deny env allowlist (the server has no interactive prompt).
    allow = parse_allowlist(os.environ.get("SMOOTH_EXTENSIONS_ALLOW"))
    if not allow:
        return None  # default deny — never spawn anything

    # `SMOOTH_EXTENSIONS_DIR` overrides the discovery dir; else the engine default.
    dir_override = (os.environ.get("SMOOTH_EXTENSIONS_DIR") or "").strip()
    global_dir = Path(dir_override) if dir_override else default_global_dir()
    # The server has no per-session workspace; project-scoped discovery keys off the
    # process cwd's `.smooth/extensions`. Usually absent -> global only.
    project = project_dir(Path.cwd())
    discovered, disc_failures = discover(global_dir, project)
    for src, err in disc_failures:
        _log.warning("sep: extension manifest failed to parse: %s (%s)", src, err)

    allowed: list[DiscoveredExtension] = []
    for ext in discovered:
        if ext.manifest.name in allow:
            allowed.append(ext)
        else:
            _log.info("sep: skipping extension %s not in SMOOTH_EXTENSIONS_ALLOW", ext.manifest.name)
    if not allowed:
        return None

    host_info = HostInfo(name="smooth-operator-server", version="0.1.0")
    # Allowlisted => trusted (the allowlist is the trust decision); project-scoped
    # extensions load because `trusted` is True.
    workspace = WorkspaceInfo(root=str(Path.cwd()), trusted=True)
    delegate = ConfirmUiProvider(sink, request_id, session_id, confirmations)

    host, load_failures = await ExtensionHost.load(allowed, host_info, workspace, UI_MODE, ["confirm"], delegate)
    for name, err in load_failures:
        _log.warning("sep: extension failed to load: %s (%s)", name, err)
    if host.is_empty():
        return None
    _log.info("sep: attached extension host to the turn (%s)", host.names())
    return ExtensionTurn(host=host, session_id=session_id, confirmations=confirmations)
