"""Coding toolset (th-82ad57) — mirrors ``go/server/coding_tools_test.go``.

The path-confinement cases are the security-relevant ones: a prompt-injected agent
must not be able to read or write outside the workspace root.
"""

from __future__ import annotations

import os

import pytest
from smooth_operator_core import FunctionTool

from smooth_operator_server.coding_tools import coding_tools, coding_tools_from_env, resolve_workspace_path


def tool_by_name(tools: list[FunctionTool], name: str) -> FunctionTool:
    for tool in tools:
        if tool.name == name:
            return tool
    raise AssertionError(f"tool {name!r} not found")


def test_coding_tools_set(tmp_path):
    tools = coding_tools(str(tmp_path))
    want = ["read_file", "write_file", "edit_file", "list_files", "grep", "bash"]
    assert [t.name for t in tools] == want


async def test_write_then_read_file(tmp_path):
    tools = coding_tools(str(tmp_path))

    res = await tool_by_name(tools, "write_file").execute({"path": "hello.txt", "content": "WORLD"})
    assert "Wrote" in res
    # The file actually landed on disk with the exact bytes.
    assert (tmp_path / "hello.txt").read_text() == "WORLD"
    # read_file returns line-numbered content.
    rd = await tool_by_name(tools, "read_file").execute({"path": "hello.txt"})
    assert "WORLD" in rd
    assert rd.startswith("     1\t")


async def test_read_file_window_and_past_eof(tmp_path):
    tools = coding_tools(str(tmp_path))
    (tmp_path / "many.txt").write_text("a\nb\nc\nd")

    windowed = await tool_by_name(tools, "read_file").execute({"path": "many.txt", "offset": 2, "limit": 2})
    assert windowed == "     2\tb\n     3\tc\n"

    past_eof = await tool_by_name(tools, "read_file").execute({"path": "many.txt", "offset": 99})
    assert past_eof.startswith("(no lines:")


async def test_read_file_missing(tmp_path):
    tools = coding_tools(str(tmp_path))
    with pytest.raises(ValueError):
        await tool_by_name(tools, "read_file").execute({"path": "nope.txt"})


async def test_read_file_requires_path(tmp_path):
    tools = coding_tools(str(tmp_path))
    with pytest.raises(ValueError):
        await tool_by_name(tools, "read_file").execute({})


async def test_write_file_creates_parent_dirs(tmp_path):
    tools = coding_tools(str(tmp_path))
    await tool_by_name(tools, "write_file").execute({"path": "a/b/c.txt", "content": "deep"})
    assert (tmp_path / "a" / "b" / "c.txt").read_text() == "deep"


async def test_edit_file(tmp_path):
    tools = coding_tools(str(tmp_path))
    edit = tool_by_name(tools, "edit_file")
    (tmp_path / "f.txt").write_text("foo bar foo")

    # Non-unique old_string without replace_all is refused.
    with pytest.raises(ValueError):
        await edit.execute({"path": "f.txt", "old_string": "foo", "new_string": "X"})
    assert (tmp_path / "f.txt").read_text() == "foo bar foo"

    # replace_all rewrites every occurrence.
    await edit.execute({"path": "f.txt", "old_string": "foo", "new_string": "X", "replace_all": True})
    assert (tmp_path / "f.txt").read_text() == "X bar X"

    # Missing old_string is an error.
    with pytest.raises(ValueError):
        await edit.execute({"path": "f.txt", "old_string": "nope", "new_string": "Y"})


async def test_edit_file_unique_match(tmp_path):
    tools = coding_tools(str(tmp_path))
    (tmp_path / "u.txt").write_text("alpha beta")
    res = await tool_by_name(tools, "edit_file").execute({"path": "u.txt", "old_string": "beta", "new_string": "gamma"})
    assert "1 replacement" in res
    assert (tmp_path / "u.txt").read_text() == "alpha gamma"


async def test_list_and_grep(tmp_path):
    tools = coding_tools(str(tmp_path))
    (tmp_path / "one.txt").write_text("needle here\nplain")
    (tmp_path / "node_modules" / "junk").mkdir(parents=True)
    (tmp_path / "node_modules" / "junk" / "x.txt").write_text("needle")

    ls = await tool_by_name(tools, "list_files").execute({})
    assert "one.txt" in ls
    assert "node_modules" not in ls

    gr = await tool_by_name(tools, "grep").execute({"pattern": "needle"})
    assert "one.txt:1:" in gr
    assert "node_modules" not in gr


async def test_list_files_empty_and_bad_dir(tmp_path):
    tools = coding_tools(str(tmp_path))
    (tmp_path / "sub").mkdir()
    assert (await tool_by_name(tools, "list_files").execute({"path": "sub"})).startswith("(empty:")
    with pytest.raises(ValueError):
        await tool_by_name(tools, "list_files").execute({"path": "missing"})


async def test_grep_no_match_and_bad_regex(tmp_path):
    tools = coding_tools(str(tmp_path))
    (tmp_path / "a.txt").write_text("nothing to see")
    assert "(no matches for" in await tool_by_name(tools, "grep").execute({"pattern": "zzz"})
    with pytest.raises(ValueError):
        await tool_by_name(tools, "grep").execute({"pattern": "("})


async def test_bash_runs_in_workspace(tmp_path):
    tools = coding_tools(str(tmp_path))
    res = await tool_by_name(tools, "bash").execute({"command": "echo hi > out.txt && cat out.txt"})
    assert "exit: 0" in res
    assert "hi" in res
    assert (tmp_path / "out.txt").read_text().strip() == "hi"


async def test_bash_nonzero_exit(tmp_path):
    tools = coding_tools(str(tmp_path))
    res = await tool_by_name(tools, "bash").execute({"command": "exit 3"})
    assert "exit: 3" in res


async def test_bash_timeout(tmp_path):
    tools = coding_tools(str(tmp_path))
    res = await tool_by_name(tools, "bash").execute({"command": "sleep 5", "timeout": 1})
    assert "killed (timeout after 1s)" in res


async def test_bash_blocks_catastrophic(tmp_path):
    tools = coding_tools(str(tmp_path))
    bash = tool_by_name(tools, "bash")
    for command in ("rm -rf /", "rm -rf $HOME/", ":(){ :|:& };:", "mkfs.ext4 /dev/sda1"):
        assert "BLOCKED" in await bash.execute({"command": command}), command


def test_path_confinement(tmp_path):
    ws = str(tmp_path)
    for bad in ("/etc/passwd", "../../etc/passwd", "", "sub/../../escape", f"{ws}/../sneaky"):
        with pytest.raises(ValueError):
            resolve_workspace_path(ws, bad)
    # Contained relative + absolute-within are both allowed.
    assert resolve_workspace_path(ws, "sub/file.txt") == os.path.join(ws, "sub", "file.txt")
    assert resolve_workspace_path(ws, os.path.join(ws, "in.txt")) == os.path.join(ws, "in.txt")
    # A path that dips out and comes back is fine — it resolves inside.
    assert resolve_workspace_path(ws, "sub/../in.txt") == os.path.join(ws, "in.txt")


@pytest.mark.parametrize(
    "tool_name,args",
    [
        ("read_file", {"path": "../escape.txt"}),
        ("write_file", {"path": "../escape.txt", "content": "x"}),
        ("edit_file", {"path": "/etc/passwd", "old_string": "root", "new_string": "pwned"}),
        ("list_files", {"path": "../"}),
        ("grep", {"pattern": "x", "path": "/etc"}),
    ],
)
async def test_tools_reject_escape(tmp_path, tool_name, args):
    """Every path-taking tool refuses to leave the workspace — the trust boundary."""
    ws = tmp_path / "ws"
    ws.mkdir()
    (tmp_path / "escape.txt").write_text("outside")
    with pytest.raises(ValueError):
        await tool_by_name(coding_tools(str(ws)), tool_name).execute(args)
    assert (tmp_path / "escape.txt").read_text() == "outside"


def test_coding_tools_from_env(tmp_path, monkeypatch):
    monkeypatch.delenv("SMOOTH_NO_TOOLS", raising=False)
    monkeypatch.setenv("SMOOTH_WORKSPACE", str(tmp_path))
    assert len(coding_tools_from_env()) == 6

    monkeypatch.setenv("SMOOTH_NO_TOOLS", "1")
    assert coding_tools_from_env() == []

    # Only the exact "1" opts out (mirrors the Go gate).
    monkeypatch.setenv("SMOOTH_NO_TOOLS", "0")
    assert len(coding_tools_from_env()) == 6

    # No SMOOTH_WORKSPACE → cwd.
    monkeypatch.delenv("SMOOTH_NO_TOOLS", raising=False)
    monkeypatch.delenv("SMOOTH_WORKSPACE", raising=False)
    monkeypatch.chdir(tmp_path)
    assert len(coding_tools_from_env()) == 6
