---
"@smooai/smooth-operator": minor
---

Python server: host SEP extensions in a turn (ui/* producer) — pearl th-66251a.

Wires the engine's `ExtensionHost` (ported to the Python core in smooth-operator-core#33) into the Python operator server, the Python sibling of the Rust reference server wiring (#159). A turn can now host `extension.toml` extensions: their tools reach the agent and their `ui/confirm` bridges onto the chat-native confirmation frame.

- **Trust — default deny.** `SMOOTH_EXTENSIONS_ALLOW` (comma-separated names) IS the trust decision; empty/unset (the default) means no extension is ever spawned and the host is never built, so behavior is byte-for-byte unchanged. `SMOOTH_EXTENSIONS_DIR` overrides the discovery dir.
- **Tools + `enabled_tools` parity.** An allowlisted extension's eager tools are added to the turn's tool set and flow through the SAME per-agent `enabled_tools` filter (`filter_tools`, by tool name) the built-ins get — so an allow-list drops an extension tool (`echo.say`) exactly like a built-in.
- **`ui/confirm` → the confirmation frame.** `ConfirmUiProvider` (a `HostDelegate`) projects an extension's `ui/confirm` onto the existing `write_confirmation_required` / `confirm_tool_action` frames via the same session-keyed `ConfirmationRegistry` the native write HITL uses; every other `ui/*` degrades headless (interactive → `{cancelled}`, render-only → `{}`). Only the `confirm` capability is advertised at handshake.
- **Teardown.** The per-turn host is shut down (subprocesses stopped, parked confirmation cleared) at turn end.

New module `smooth_operator_server.extensions` (`build_extension_host`, `ConfirmUiProvider`, `parse_allowlist`), wired into `turn_runner.py`. Integration tests drive a real echo-peer extension through a live `send_message` turn (tool runs + result streams back) and assert `enabled_tools` filtering parity, plus the `ui/confirm` bridge unit tests.
