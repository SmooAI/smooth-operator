using System.Text.Json;
using Microsoft.Extensions.AI;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// Tests for the workspace-confined coding toolset (th-82ad57). Mirrors the Go server's
/// <c>coding_tools_test.go</c>: the set registers under the six shared tool names, each tool works on
/// its happy path and errors on its failure path, and — the load-bearing property — every path the
/// model supplies is confined to the workspace, so an escape (<c>../</c>, an absolute path outside
/// the root) is rejected before any filesystem access happens.
/// </summary>
public class CodingToolsTests : IDisposable
{
    private readonly string _ws = Directory.CreateTempSubdirectory("smooth-coding-tools").FullName;
    private readonly IReadOnlyList<AITool> _tools;

    public CodingToolsTests() => _tools = CodingTools.Create(_ws);

    public void Dispose()
    {
        GC.SuppressFinalize(this);
        try
        {
            Directory.Delete(_ws, recursive: true);
        }
        catch (IOException)
        {
            // best effort — a temp dir left behind must not fail the suite
        }
    }

    private AIFunction Tool(string name)
    {
        var tool = _tools.FirstOrDefault(t => t.Name == name);
        Assert.NotNull(tool);
        return (AIFunction)tool!;
    }

    private async Task<string> Call(string name, params (string Key, object Value)[] args)
    {
        var arguments = new AIFunctionArguments();
        foreach (var (key, value) in args)
        {
            arguments[key] = value;
        }

        return (await Tool(name).InvokeAsync(arguments))?.ToString() ?? string.Empty;
    }

    /// <summary>Assert the call was refused BY THE PATH GUARD, not by some incidental failure.</summary>
    private async Task AssertConfined(string name, params (string Key, object Value)[] args)
    {
        var ex = await Assert.ThrowsAsync<ArgumentException>(() => Call(name, args));
        Assert.Contains("outside the workspace", ex.Message, StringComparison.Ordinal);
    }

    private string Path(params string[] parts) => System.IO.Path.Combine([_ws, .. parts]);

    [Fact]
    public void Set_HasTheSixSharedTools()
    {
        string[] want = ["read_file", "write_file", "edit_file", "list_files", "grep", "bash"];
        Assert.Equal(want.Length, _tools.Count);
        Assert.Equal(want.Order(StringComparer.Ordinal), _tools.Select(t => t.Name).Order(StringComparer.Ordinal));
    }

    [Fact]
    public void Set_UsesTheSharedArgumentNames()
    {
        // The JSON schema the model sees must match the Go/Rust/Python/TS ports argument-for-argument,
        // or a cross-engine bench prompt calls a tool with arguments this host silently drops.
        static IEnumerable<string> Props(AIFunction f) =>
            f.JsonSchema.TryGetProperty("properties", out var p)
                ? p.EnumerateObject().Select(x => x.Name)
                : [];

        Assert.Equal(["path", "offset", "limit"], Props(Tool("read_file")));
        Assert.Equal(["path", "content"], Props(Tool("write_file")));
        Assert.Equal(["path", "old_string", "new_string", "replace_all"], Props(Tool("edit_file")));
        Assert.Equal(["path"], Props(Tool("list_files")));
        Assert.Equal(["pattern", "path"], Props(Tool("grep")));
        Assert.Equal(["command", "timeout"], Props(Tool("bash")));
    }

    [Fact]
    public async Task WriteThenRead()
    {
        var res = await Call("write_file", ("path", "hello.txt"), ("content", "WORLD"));
        Assert.Contains("Wrote", res, StringComparison.Ordinal);
        Assert.Equal("WORLD", File.ReadAllText(Path("hello.txt")));

        var read = await Call("read_file", ("path", "hello.txt"));
        Assert.Contains("WORLD", read, StringComparison.Ordinal);
        Assert.Contains("     1\t", read, StringComparison.Ordinal); // line-numbered like the Go port
    }

    [Fact]
    public async Task ReadFile_WindowsLinesAndReportsEmptyWindow()
    {
        await Call("write_file", ("path", "many.txt"), ("content", "a\nb\nc\nd"));
        var window = await Call("read_file", ("path", "many.txt"), ("offset", 2), ("limit", 2));
        Assert.Contains("b", window, StringComparison.Ordinal);
        Assert.Contains("c", window, StringComparison.Ordinal);
        Assert.DoesNotContain("d", window, StringComparison.Ordinal);

        var past = await Call("read_file", ("path", "many.txt"), ("offset", 99));
        Assert.Contains("no lines", past, StringComparison.Ordinal);
    }

    [Fact]
    public async Task ReadFile_MissingFile_Errors() =>
        await Assert.ThrowsAnyAsync<Exception>(() => Call("read_file", ("path", "nope.txt")));

    [Fact]
    public async Task WriteFile_CreatesParentDirs()
    {
        await Call("write_file", ("path", "a/b/c.txt"), ("content", "deep"));
        Assert.Equal("deep", File.ReadAllText(Path("a", "b", "c.txt")));
    }

    [Fact]
    public async Task EditFile_UniqueMatch_NonUniqueRefused_MissingErrors()
    {
        File.WriteAllText(Path("f.txt"), "foo bar foo");

        // Non-unique old_string without replace_all is refused (no write happens).
        await Assert.ThrowsAnyAsync<Exception>(() =>
            Call("edit_file", ("path", "f.txt"), ("old_string", "foo"), ("new_string", "X")));
        Assert.Equal("foo bar foo", File.ReadAllText(Path("f.txt")));

        // replace_all rewrites every occurrence.
        await Call("edit_file", ("path", "f.txt"), ("old_string", "foo"), ("new_string", "X"), ("replace_all", true));
        Assert.Equal("X bar X", File.ReadAllText(Path("f.txt")));

        // A unique match replaces just it.
        await Call("edit_file", ("path", "f.txt"), ("old_string", "bar"), ("new_string", "baz"));
        Assert.Equal("X baz X", File.ReadAllText(Path("f.txt")));

        // Missing old_string is an error.
        await Assert.ThrowsAnyAsync<Exception>(() =>
            Call("edit_file", ("path", "f.txt"), ("old_string", "nope"), ("new_string", "Y")));
    }

    [Fact]
    public async Task ListAndGrep_SkipNoiseDirs()
    {
        File.WriteAllText(Path("one.txt"), "needle here\nplain");
        Directory.CreateDirectory(Path("node_modules", "junk"));
        File.WriteAllText(Path("node_modules", "junk", "x.txt"), "needle");

        var list = await Call("list_files");
        Assert.Contains("one.txt", list, StringComparison.Ordinal);
        Assert.DoesNotContain("node_modules", list, StringComparison.Ordinal);

        var grep = await Call("grep", ("pattern", "needle"));
        Assert.Contains("one.txt:1:", grep, StringComparison.Ordinal);
        Assert.DoesNotContain("node_modules", grep, StringComparison.Ordinal);

        Assert.Contains("no matches", await Call("grep", ("pattern", "zzz-not-here")), StringComparison.Ordinal);
        await Assert.ThrowsAnyAsync<Exception>(() => Call("grep", ("pattern", "(unclosed")));
    }

    [Fact]
    public async Task ListFiles_MarksDirectoriesAndReportsEmpty()
    {
        Directory.CreateDirectory(Path("sub"));
        File.WriteAllText(Path("sub", "in.txt"), "x");
        var list = await Call("list_files");
        Assert.Contains("sub/", list, StringComparison.Ordinal);
        Assert.Contains(System.IO.Path.Combine("sub", "in.txt"), list, StringComparison.Ordinal);

        Directory.CreateDirectory(Path("empty"));
        Assert.Contains("empty:", await Call("list_files", ("path", "empty")), StringComparison.Ordinal);
    }

    [Fact]
    public async Task Bash_RunsInWorkspace()
    {
        var res = await Call("bash", ("command", "echo hi > out.txt && cat out.txt"));
        Assert.Contains("exit: 0", res, StringComparison.Ordinal);
        Assert.Contains("hi", res, StringComparison.Ordinal);
        Assert.Equal("hi", File.ReadAllText(Path("out.txt")).Trim());
    }

    [Fact]
    public async Task Bash_ReportsNonZeroExitAndStderr()
    {
        var res = await Call("bash", ("command", "echo boom >&2; exit 3"));
        Assert.Contains("exit: 3", res, StringComparison.Ordinal);
        Assert.Contains("boom", res, StringComparison.Ordinal);
    }

    [Fact]
    public async Task Bash_KillsOnTimeout()
    {
        var res = await Call("bash", ("command", "sleep 5"), ("timeout", 1));
        Assert.Contains("killed (timeout", res, StringComparison.Ordinal);
    }

    [Fact]
    public async Task Bash_BlocksCatastrophicCommands()
    {
        Assert.Contains("BLOCKED", await Call("bash", ("command", "rm -rf /")), StringComparison.Ordinal);
        Assert.Contains("BLOCKED", await Call("bash", ("command", "mkfs.ext4 /dev/sda1")), StringComparison.Ordinal);
        // A workspace-local rm is still allowed — the deny is for the unrecoverable ones only.
        Assert.Contains("exit: 0", await Call("bash", ("command", "rm -rf ./scratch")), StringComparison.Ordinal);
    }

    // ── The trust boundary ────────────────────────────────────────────────────────────────────

    [Fact]
    public void ResolveWorkspacePath_RejectsEscapes()
    {
        Assert.Throws<ArgumentException>(() => CodingTools.ResolveWorkspacePath(_ws, "/etc/passwd"));
        Assert.Throws<ArgumentException>(() => CodingTools.ResolveWorkspacePath(_ws, "../../etc/passwd"));
        Assert.Throws<ArgumentException>(() => CodingTools.ResolveWorkspacePath(_ws, "sub/../../escape"));
        Assert.Throws<ArgumentException>(() => CodingTools.ResolveWorkspacePath(_ws, ""));
        // A sibling dir sharing the root's name prefix is NOT inside it.
        Assert.Throws<ArgumentException>(() => CodingTools.ResolveWorkspacePath(_ws, _ws + "-evil/x"));
    }

    [Fact]
    public void ResolveWorkspacePath_AllowsContainedPaths()
    {
        Assert.Equal(Path("sub", "file.txt"), CodingTools.ResolveWorkspacePath(_ws, "sub/file.txt"));
        Assert.Equal(Path("in.txt"), CodingTools.ResolveWorkspacePath(_ws, Path("in.txt")));
        Assert.Equal(Path("a"), CodingTools.ResolveWorkspacePath(_ws, "./b/../a"));
    }

    [Fact]
    public async Task Tools_RejectPathsEscapingTheWorkspace()
    {
        // Unique names: the assertions below are about THIS run's escape attempts, so a stray file
        // in the shared temp dir must never make them pass or fail by accident.
        var outside = System.IO.Path.Combine(System.IO.Path.GetTempPath(), $"smooth-escape-target-{Guid.NewGuid():N}.txt");
        var escapee = $"smooth-escapee-{Guid.NewGuid():N}.txt";
        File.WriteAllText(outside, "secret");
        try
        {
            await AssertConfined("write_file", ("path", $"../{escapee}"), ("content", "x"));
            await AssertConfined("read_file", ("path", outside));
            await AssertConfined("read_file", ("path", "/etc/passwd"));
            await AssertConfined("edit_file", ("path", outside), ("old_string", "secret"), ("new_string", "x"));
            await AssertConfined("list_files", ("path", ".."));
            await AssertConfined("grep", ("pattern", "secret"), ("path", ".."));

            Assert.Equal("secret", File.ReadAllText(outside)); // nothing outside the workspace was touched
            Assert.False(File.Exists(System.IO.Path.Combine(_ws, "..", escapee)));
        }
        finally
        {
            File.Delete(outside);
        }
    }

    [Fact]
    public void JsonSchema_IsWellFormed()
    {
        // Every tool must present a serializable object schema — a malformed one breaks tool
        // advertisement for the whole turn, not just that tool.
        foreach (var tool in _tools.Cast<AIFunction>())
        {
            Assert.Equal(JsonValueKind.Object, tool.JsonSchema.ValueKind);
            Assert.Equal("object", tool.JsonSchema.GetProperty("type").GetString());
            Assert.False(string.IsNullOrWhiteSpace(tool.Description));
        }
    }
}
