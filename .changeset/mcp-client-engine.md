---
'@smooai/smooth-operator': minor
---

MCP client: configured Model Context Protocol servers now surface as engine tools.

New `mcp` module with `McpConfig` / `McpServerConfig` (the `[[servers]]` TOML shape Smooth already writes to `~/.smooth/mcp.toml`, extended with `url` + `bearer_token` for streamable HTTP) and `McpToolProvider`, a `ToolProvider` that connects each server, lists its tools, and registers them as `mcp__<server>__<tool>`. Because it goes through the existing `ToolProvider` seam, MCP tools land in the same per-turn `ToolRegistry` as the built-ins, so a host's `ToolHook`s (permission gate, Narc) apply with no extra wiring. `ChainedToolProvider` composes a host's own provider with the MCP one.

Both transports are supported — stdio (spawned child process) and streamable HTTP with an optional bearer token. `env`, `args`, `url` and `bearer_token` expand `${env:VAR}` at connect time so secrets stay out of config files. Calls are bounded by a per-server timeout (60s default) and MCP tool-level errors map to engine tool errors. A server that will not connect is logged and skipped — its tools are simply absent from the turn, never a crash.

Tools only in this version: resources, prompts, sampling, and `notifications/tools/list_changed` are not implemented.
