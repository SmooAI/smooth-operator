#!/usr/bin/env python3
"""A dependency-free SEP echo peer used as the server's extension-hosting test
target. Speaks JSON-RPC 2.0 ndjson over stdin/stdout — a real extension subprocess.
Mirrors ``spec/extension/conformance/echo.mjs`` (kept in Python so the server's test
lane needs no node)."""

import json
import sys


def reply(rid, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": rid, "result": result}) + "\n")
    sys.stdout.flush()


def reply_error(rid, code, message):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": rid, "error": {"code": code, "message": message}}) + "\n")
    sys.stdout.flush()


def main() -> None:
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        frame = json.loads(line)
        rid = frame.get("id")
        method = frame.get("method")
        params = frame.get("params") or {}
        is_notification = rid is None

        if method == "initialize":
            reply(
                rid,
                {
                    "protocol_version": min(params.get("protocol_version", 1), 1),
                    "extension": {"name": "echo", "version": "0.1.0"},
                    "registrations": {
                        "tools": [
                            {
                                "name": "say",
                                "description": "Echo a phrase back.",
                                "parameters": {
                                    "type": "object",
                                    "properties": {"phrase": {"type": "string"}},
                                    "required": ["phrase"],
                                },
                            }
                        ],
                        "subscriptions": ["turn_start", "turn_end"],
                    },
                },
            )
        elif method == "ping":
            reply(rid, {})
        elif method == "hook":
            reply(rid, {"action": "continue"})
        elif method == "tool/execute":
            phrase = (params.get("arguments") or {}).get("phrase", "")
            reply(rid, {"content": phrase, "is_error": False})
        elif method == "shutdown":
            reply(rid, {})
            sys.exit(0)
        elif method in ("event", "$/cancel"):
            pass
        else:
            if not is_notification:
                reply_error(rid, -32601, f"method not found: {method}")


if __name__ == "__main__":
    main()
