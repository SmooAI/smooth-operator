"""Coding toolset (th-82ad57) — the file-editing tools the LocalServer agent needs to
actually do work. Without these, ``serve_local`` spins up a chat-only agent that replies
"I don't have file editing tools" and scores structural 0% on the parity bench.

A faithful port of ``go/server/coding_tools.go`` (itself mirroring the Rust daemon's
``crates/smooth-tools``): read_file, write_file, edit_file, list_files, grep, bash — all
confined to a single workspace root. Every filesystem path the model supplies is routed
through :func:`resolve_workspace_path` so a prompt-injected agent can't read or write
outside the workspace (a trust boundary — not simplified away).

ponytail: bash here runs unsandboxed rooted at the workspace (the Python host has no
kernel sandbox), matching the Go host. Acceptable for the single-trusted-user
loopback/bench flavor. Upgrade path: wrap a SandboxedCommand equivalent if this ever
serves untrusted callers.
"""

from __future__ import annotations

import asyncio
import os
import re
from typing import Any

from smooth_operator_core import FunctionTool

#: read_file: max lines returned when no limit given.
READ_DEFAULT_LIMIT = 2000
#: list_files: max entries returned.
LIST_CAP = 200
#: list_files / grep: max entries examined before stopping.
LIST_WALK_BUDGET = 50000
#: grep: max matching lines returned.
GREP_MATCH_CAP = 200
#: bash: max bytes returned per stream.
BASH_OUTPUT_CAP = 50000
#: bash: default timeout (seconds) if none given.
BASH_DEFAULT_KILL = 120

#: Directories never descended into by list_files / grep.
SKIP_DIRS = frozenset({".git", "node_modules", "target"})


def resolve_workspace_path(base: str, rel: str) -> str:
    """Confine ``rel`` to ``base`` lexically (no symlink following, no existence
    requirement). Accepts a relative path (joined onto ``base``) or an absolute path
    that lexically resolves inside ``base``. Rejects empty paths and any path that
    escapes ``base`` after collapsing ``.`` / ``..``. Mirrors the Go/Rust guard.

    Raises ``ValueError`` on rejection — the engine turns a tool exception into an
    error string fed back to the model, so a rejected path never touches disk.
    """
    if not rel:
        raise ValueError("empty path")
    base_norm = os.path.normpath(base)
    joined = os.path.normpath(rel) if os.path.isabs(rel) else os.path.normpath(os.path.join(base_norm, rel))
    # normpath collapses "..", so a contained path is base_norm itself or has base_norm+sep as prefix.
    if joined != base_norm and not joined.startswith(base_norm + os.sep):
        raise ValueError(f"path {rel!r} is outside the workspace (resolved to {joined}, not under {base_norm})")
    return joined


def _req_str(args: dict[str, Any], key: str) -> str:
    """Pull a required string argument from a tool-call args map."""
    if key not in args:
        raise ValueError(f"missing required argument {key!r}")
    value = args[key]
    if not isinstance(value, str):
        raise ValueError(f"argument {key!r} must be a string")
    return value


def _opt_int(args: dict[str, Any], key: str) -> int | None:
    """Pull an optional integer argument (JSON numbers may decode as float)."""
    value = args.get(key)
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return int(value)


def _read_file_tool(workspace: str) -> FunctionTool:
    async def _run(args: dict[str, Any]) -> str:
        rel = _req_str(args, "path")
        path = resolve_workspace_path(workspace, rel)
        try:
            with open(path, "rb") as fh:
                data = fh.read()
        except OSError as exc:
            raise ValueError(f"read {rel}: {exc}") from exc
        offset = _opt_int(args, "offset") or 1
        if offset <= 0:
            offset = 1
        limit = _opt_int(args, "limit") or READ_DEFAULT_LIMIT
        if limit <= 0:
            limit = READ_DEFAULT_LIMIT
        lines = data.decode("utf-8", errors="replace").split("\n")
        window = lines[offset - 1 : offset - 1 + limit]
        if not window:
            return f"(no lines: file has {len(lines)} line(s), offset {offset})"
        return "".join(f"{offset + i:6d}\t{line}\n" for i, line in enumerate(window))

    return FunctionTool(
        name="read_file",
        description="Read a UTF-8 text file within the workspace. Returns line-numbered content; supports an optional line window.",
        parameters={
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Relative path within the workspace"},
                "offset": {"type": "integer", "description": "1-based start line (default: 1)"},
                "limit": {"type": "integer", "description": "Max lines to return (default: 2000)"},
            },
            "required": ["path"],
        },
        func=_run,
    )


def _write_file_tool(workspace: str) -> FunctionTool:
    async def _run(args: dict[str, Any]) -> str:
        rel = _req_str(args, "path")
        content = _req_str(args, "content")
        path = resolve_workspace_path(workspace, rel)
        payload = content.encode("utf-8")
        try:
            os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
            with open(path, "wb") as fh:
                fh.write(payload)
        except OSError as exc:
            raise ValueError(f"write {rel}: {exc}") from exc
        return f"Wrote {len(payload)} bytes to {rel}"

    return FunctionTool(
        name="write_file",
        description="Create or overwrite a file in the workspace with the given content (parent dirs are created).",
        parameters={
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Relative path within the workspace"},
                "content": {"type": "string", "description": "Full file content to write"},
            },
            "required": ["path", "content"],
        },
        func=_run,
    )


def _edit_file_tool(workspace: str) -> FunctionTool:
    async def _run(args: dict[str, Any]) -> str:
        rel = _req_str(args, "path")
        old_string = _req_str(args, "old_string")
        new_string = _req_str(args, "new_string")
        path = resolve_workspace_path(workspace, rel)
        try:
            with open(path, "rb") as fh:
                text = fh.read().decode("utf-8", errors="replace")
        except OSError as exc:
            raise ValueError(f"read {rel}: {exc}") from exc
        count = text.count(old_string)
        if count == 0:
            raise ValueError(f"old_string not found in {rel}")
        replace_all = args.get("replace_all") is True
        if not replace_all and count > 1:
            raise ValueError(f"old_string occurs {count} times in {rel}; pass replace_all or supply a unique string")
        updated = text.replace(old_string, new_string) if replace_all else text.replace(old_string, new_string, 1)
        try:
            with open(path, "wb") as fh:
                fh.write(updated.encode("utf-8"))
        except OSError as exc:
            raise ValueError(f"write {rel}: {exc}") from exc
        return f"Edited {rel} ({count} replacement(s))"

    return FunctionTool(
        name="edit_file",
        description="Replace an exact substring in a workspace file. old_string must occur exactly once (unless replace_all is true).",
        parameters={
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Relative path within the workspace"},
                "old_string": {"type": "string", "description": "Exact text to replace"},
                "new_string": {"type": "string", "description": "Replacement text"},
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace every occurrence (default: false, requires a unique match)",
                },
            },
            "required": ["path", "old_string", "new_string"],
        },
        func=_run,
    )


def _list_files_tool(workspace: str) -> FunctionTool:
    async def _run(args: dict[str, Any]) -> str:
        rel = args.get("path") or "."
        if not isinstance(rel, str):
            raise ValueError("argument 'path' must be a string")
        root = resolve_workspace_path(workspace, rel)
        if not os.path.isdir(root):
            raise ValueError(f"list {rel}: not a directory")
        entries: list[str] = []
        examined = 0
        for dirpath, dirnames, filenames in os.walk(root):
            dirnames[:] = sorted(d for d in dirnames if d not in SKIP_DIRS)
            for name, is_dir in [(d, True) for d in dirnames] + [(f, False) for f in sorted(filenames)]:
                examined += 1
                if examined > LIST_WALK_BUDGET or len(entries) >= LIST_CAP:
                    break
                rel_path = os.path.relpath(os.path.join(dirpath, name), workspace)
                entries.append(rel_path + "/" if is_dir else rel_path)
            if examined > LIST_WALK_BUDGET or len(entries) >= LIST_CAP:
                break
        if not entries:
            return f"(empty: {rel})"
        out = "\n".join(sorted(entries))
        if len(entries) >= LIST_CAP:
            out += f"\n... (truncated at {LIST_CAP} entries)"
        return out

    return FunctionTool(
        name="list_files",
        description="List files and directories under a workspace path (recursive, skips .git/node_modules/target). Relative paths.",
        parameters={
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Relative directory to list (default: workspace root)"},
            },
        },
        func=_run,
    )


def _grep_tool(workspace: str) -> FunctionTool:
    async def _run(args: dict[str, Any]) -> str:
        pattern = _req_str(args, "pattern")
        try:
            regex = re.compile(pattern)
        except re.error as exc:
            raise ValueError(f"invalid regexp: {exc}") from exc
        rel = args.get("path") or "."
        if not isinstance(rel, str):
            raise ValueError("argument 'path' must be a string")
        root = resolve_workspace_path(workspace, rel)
        matches: list[str] = []
        examined = 0
        for dirpath, dirnames, filenames in os.walk(root):
            dirnames[:] = sorted(d for d in dirnames if d not in SKIP_DIRS)
            for name in sorted(filenames):
                examined += 1
                if examined > LIST_WALK_BUDGET or len(matches) >= GREP_MATCH_CAP:
                    break
                path = os.path.join(dirpath, name)
                try:
                    with open(path, "rb") as fh:
                        text = fh.read().decode("utf-8", errors="replace")
                except OSError:
                    continue  # skip unreadable entries, keep walking
                rel_path = os.path.relpath(path, workspace)
                for i, line in enumerate(text.split("\n")):
                    if regex.search(line):
                        matches.append(f"{rel_path}:{i + 1}:{line}")
                        if len(matches) >= GREP_MATCH_CAP:
                            break
            if examined > LIST_WALK_BUDGET or len(matches) >= GREP_MATCH_CAP:
                break
        if not matches:
            return f"(no matches for {pattern!r})"
        out = "\n".join(matches)
        if len(matches) >= GREP_MATCH_CAP:
            out += f"\n... (truncated at {GREP_MATCH_CAP} matches)"
        return out

    return FunctionTool(
        name="grep",
        description="Search workspace file contents with a regular expression. Returns path:line:text matches (skips .git/node_modules/target).",
        parameters={
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regexp to search for"},
                "path": {"type": "string", "description": "Relative directory to search (default: workspace root)"},
            },
            "required": ["pattern"],
        },
        func=_run,
    )


#: A cheap defense-in-depth deny for the handful of commands that are unrecoverable
#: regardless of workspace confinement (the Python host has no kernel sandbox).
#: Mirrors the Go/Rust bash circuit-breaker. NOT a substitute for a real sandbox.
CATASTROPHIC_BASH = re.compile(r"rm\s+-\S*[rf]\S*\s+(/|~|\$HOME)(\s|/|;|$)|:\s*\(\s*\)\s*\{|>\s*/dev/sd|mkfs")


def _bash_tool(workspace: str) -> FunctionTool:
    async def _run(args: dict[str, Any]) -> str:
        command = _req_str(args, "command")
        if CATASTROPHIC_BASH.search(command):
            return f"BLOCKED: refused to run a catastrophic command (e.g. `rm -rf /`, fork bomb, mkfs): {command}"
        timeout = _opt_int(args, "timeout") or BASH_DEFAULT_KILL
        if timeout <= 0:
            timeout = BASH_DEFAULT_KILL
        proc = await asyncio.create_subprocess_exec(
            "sh",
            "-c",
            command,
            cwd=workspace,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.STDOUT,
        )
        timed_out = False
        try:
            out, _ = await asyncio.wait_for(proc.communicate(), timeout=timeout)
        except (asyncio.TimeoutError, TimeoutError):
            timed_out = True
            proc.kill()
            out, _ = await proc.communicate()
        if len(out) > BASH_OUTPUT_CAP:
            out = out[:BASH_OUTPUT_CAP] + b"\n... (output truncated)"
        text = out.decode("utf-8", errors="replace")
        if timed_out:
            return f"exit: killed (timeout after {timeout}s)\n{text}"
        return f"exit: {proc.returncode}\n{text}"

    return FunctionTool(
        name="bash",
        description="Run a shell command (sh -c) with the workspace as the working directory. Returns exit code, stdout, stderr.",
        parameters={
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "The shell command to run"},
                "timeout": {
                    "type": "integer",
                    "description": "Optional: max seconds before the command is killed (default 120)",
                },
            },
            "required": ["command"],
        },
        func=_run,
    )


def coding_tools(workspace: str) -> list[FunctionTool]:
    """Build the workspace-confined coding toolset for a LocalServer agent.

    Pass the result as ``ServerState.tools``. ``workspace`` is the root every file
    operation is confined to and the working directory bash commands start in (the
    serve binary launches with cwd == workspace, so ``"."`` is the natural argument).
    """
    root = os.path.normpath(os.path.abspath(workspace))
    return [
        _read_file_tool(root),
        _write_file_tool(root),
        _edit_file_tool(root),
        _list_files_tool(root),
        _grep_tool(root),
        _bash_tool(root),
    ]


def coding_tools_from_env() -> list[FunctionTool]:
    """The serve-binary env contract, shared with the sibling hosts:

    ``SMOOTH_NO_TOOLS=1`` → chat-only agent (no coding tools).
    ``SMOOTH_WORKSPACE``  → root the coding tools are confined to (default: cwd).
    """
    if os.environ.get("SMOOTH_NO_TOOLS") == "1":
        return []
    return coding_tools(os.environ.get("SMOOTH_WORKSPACE") or os.getcwd())
