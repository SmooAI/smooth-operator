---
"@smooai/smooth-operator": patch
---

Go server: host SEP extensions in a turn + ui/confirm bridge (th-829d9f).

Wires the engine's SEP `ExtensionHost` (new in smooth-operator-core) into the Go
operator server's send_message turn:

- **Default-deny discovery** — `SMOOTH_EXTENSIONS_ALLOW` (comma-separated names)
  is the trust decision; empty (the default) builds no host, so behavior is
  byte-for-byte unchanged. Allowlisted `extension.toml` extensions are discovered
  (`SMOOTH_EXTENSIONS_DIR` or the engine default) and spawned per turn.
- **Tool composition** — an extension's tools (`<ext>.<tool>`) are folded into the
  turn's tool set before the SMOODEV-590 `enabled_tools` / authLevel filter, so
  they gate exactly like a built-in tool.
- **ui/confirm bridge** — `confirmUIProvider` projects an extension's `ui/confirm`
  onto the existing `write_confirmation_required` / `confirm_tool_action` frames via
  the per-connection confirmation registry; other `ui/*` degrade headless.

Covered by an end-to-end test that drives a scripted model calling an
extension-registered tool through the real WS/dispatcher turn (echo peer via a
self-re-exec of the test binary), asserting execution and `enabled_tools` filtering
parity, plus default-deny. Race-clean.
