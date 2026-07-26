using System.ComponentModel;
using System.Diagnostics;
using System.Globalization;
using System.Text;
using System.Text.RegularExpressions;
using Microsoft.Extensions.AI;

namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// Coding toolset (th-82ad57) — the file-editing tools the local-flavor agent needs to actually do
/// work. Without these the host serves a chat-only agent that replies "I don't have file editing
/// tools" and scores a structural 0% on the parity bench.
///
/// A faithful port of the Go server's <c>CodingTools</c> (<c>go/server/coding_tools.go</c>), which in
/// turn mirrors the Rust daemon's set (<c>crates/smooth-tools</c>): <c>read_file</c>,
/// <c>write_file</c>, <c>edit_file</c>, <c>list_files</c>, <c>grep</c>, <c>bash</c> — same tool
/// names, same argument names, same limits, all confined to a single workspace root. Every
/// filesystem path the model supplies is routed through <see cref="ResolveWorkspacePath"/> so a
/// prompt-injected agent cannot read or write outside the workspace (a trust boundary — not
/// simplified away).
///
/// ponytail: bash here runs unsandboxed rooted at the workspace (the .NET host has no kernel
/// sandbox, same as the Go host). Acceptable for the single-trusted-user loopback/bench flavor.
/// Upgrade path: wrap a SandboxedCommand equivalent if this ever serves untrusted callers.
/// </summary>
public static class CodingTools
{
    /// <summary>read_file: max lines returned when no limit is given.</summary>
    public const int ReadDefaultLimit = 2000;

    /// <summary>list_files: max entries returned.</summary>
    public const int ListCap = 200;

    /// <summary>list_files / grep: max entries examined before the walk stops.</summary>
    public const int ListWalkBudget = 50000;

    /// <summary>grep: max matching lines returned.</summary>
    public const int GrepMatchCap = 200;

    /// <summary>bash: max bytes of output returned.</summary>
    public const int BashOutputCap = 50000;

    /// <summary>bash: default timeout (seconds) when none is given.</summary>
    public const int BashDefaultTimeoutSeconds = 120;

    /// <summary>Directory names never descended into by list_files / grep.</summary>
    private static readonly string[] SkipDirs = [".git", "node_modules", "target"];

    /// <summary>
    /// A cheap defense-in-depth deny for the handful of commands that are unrecoverable regardless
    /// of workspace confinement (this host has no kernel sandbox). Mirrors the Rust bash tool's
    /// circuit breaker and the Go port's <c>catastrophicBash</c>. NOT a substitute for a real sandbox.
    /// </summary>
    private static readonly Regex CatastrophicBash = new(
        @"rm\s+-\S*[rf]\S*\s+(/|~|\$HOME)(\s|/|;|$)|:\s*\(\s*\)\s*\{|>\s*/dev/sd|mkfs",
        RegexOptions.Compiled | RegexOptions.CultureInvariant);

    /// <summary>
    /// Build the workspace-confined coding toolset. Register the result as the server's
    /// <c>IReadOnlyList&lt;AITool&gt;</c> (the DI seam the FrameDispatcher reads tools from).
    /// <paramref name="workspace"/> is the root every file operation is confined to and the working
    /// directory bash commands start in.
    /// </summary>
    public static IReadOnlyList<AITool> Create(string workspace)
    {
        var root = NormalizeRoot(workspace);
        return
        [
            ReadFileTool(root),
            WriteFileTool(root),
            EditFileTool(root),
            ListFilesTool(root),
            GrepTool(root),
            BashTool(root),
        ];
    }

    /// <summary>
    /// Confine <paramref name="path"/> to <paramref name="workspace"/> lexically (no symlink
    /// following, no existence requirement). Accepts a relative path (joined onto the workspace) or
    /// an absolute path that lexically resolves inside it. Rejects empty paths and any path that
    /// escapes after collapsing "." / "..". Mirrors Rust's <c>resolve_workspace_path</c> and Go's
    /// <c>resolveWorkspacePath</c>.
    /// </summary>
    /// <exception cref="ArgumentException">The path is empty or escapes the workspace.</exception>
    public static string ResolveWorkspacePath(string workspace, string path)
    {
        if (string.IsNullOrEmpty(path))
        {
            throw new ArgumentException("empty path", nameof(path));
        }

        var root = NormalizeRoot(workspace);
        // GetFullPath collapses "." / ".." lexically and does not resolve symlinks — the same
        // normalization Go's filepath.Clean does, so the prefix check below is sound.
        var resolved = Path.GetFullPath(path, root).TrimEnd(Path.DirectorySeparatorChar);
        if (resolved.Length == 0)
        {
            resolved = Path.DirectorySeparatorChar.ToString();
        }

        if (resolved != root && !resolved.StartsWith(root + Path.DirectorySeparatorChar, StringComparison.Ordinal))
        {
            throw new ArgumentException(
                $"path '{path}' is outside the workspace (resolved to {resolved}, not under {root})", nameof(path));
        }

        return resolved;
    }

    /// <summary>Absolute, separator-normalized, trailing-separator-free form of a workspace root.</summary>
    private static string NormalizeRoot(string workspace)
    {
        var root = Path.GetFullPath(string.IsNullOrEmpty(workspace) ? "." : workspace);
        var trimmed = root.TrimEnd(Path.DirectorySeparatorChar);
        return trimmed.Length == 0 ? root : trimmed;
    }

    /// <summary>Path of <paramref name="full"/> relative to the workspace root, for display.</summary>
    private static string Relative(string root, string full) =>
        full.StartsWith(root + Path.DirectorySeparatorChar, StringComparison.Ordinal) ? full[(root.Length + 1)..] : full;

    private static AITool ReadFileTool(string root)
    {
        string Read(
            [Description("Relative path within the workspace")] string path,
            [Description("1-based start line (default: 1)")] int offset = 1,
            [Description("Max lines to return (default: 2000)")] int limit = ReadDefaultLimit)
        {
            var resolved = ResolveWorkspacePath(root, path);
            var text = File.ReadAllText(resolved);
            if (offset <= 0)
            {
                offset = 1;
            }

            if (limit <= 0)
            {
                limit = ReadDefaultLimit;
            }

            var lines = text.Split('\n');
            var sb = new StringBuilder();
            var shown = 0;
            for (var i = offset - 1; i >= 0 && i < lines.Length && shown < limit; i++, shown++)
            {
                sb.Append(CultureInfo.InvariantCulture, $"{i + 1,6}\t{lines[i]}\n");
            }

            return shown == 0
                ? string.Create(CultureInfo.InvariantCulture, $"(no lines: file has {lines.Length} line(s), offset {offset})")
                : sb.ToString();
        }

        return Tool(Read, "read_file",
            "Read a UTF-8 text file within the workspace. Returns line-numbered content; supports an optional line window.");
    }

    private static AITool WriteFileTool(string root)
    {
        string Write(
            [Description("Relative path within the workspace")] string path,
            [Description("Full file content to write")] string content)
        {
            var resolved = ResolveWorkspacePath(root, path);
            var parent = Path.GetDirectoryName(resolved);
            if (!string.IsNullOrEmpty(parent))
            {
                Directory.CreateDirectory(parent);
            }

            File.WriteAllText(resolved, content);
            return string.Create(CultureInfo.InvariantCulture, $"Wrote {Encoding.UTF8.GetByteCount(content)} bytes to {path}");
        }

        return Tool(Write, "write_file",
            "Create or overwrite a file in the workspace with the given content (parent dirs are created).");
    }

    private static AITool EditFileTool(string root)
    {
        string Edit(
            [Description("Relative path within the workspace")] string path,
            [Description("Exact text to replace")] string old_string,
            [Description("Replacement text")] string new_string,
            [Description("Replace every occurrence (default: false, requires a unique match)")] bool replace_all = false)
        {
            if (string.IsNullOrEmpty(old_string))
            {
                throw new ArgumentException("old_string must not be empty", nameof(old_string));
            }

            var resolved = ResolveWorkspacePath(root, path);
            var text = File.ReadAllText(resolved);
            var count = CountOccurrences(text, old_string);
            if (count == 0)
            {
                throw new ArgumentException($"old_string not found in {path}", nameof(old_string));
            }

            if (!replace_all && count > 1)
            {
                throw new ArgumentException(
                    string.Create(CultureInfo.InvariantCulture,
                        $"old_string occurs {count} times in {path}; pass replace_all or supply a unique string"),
                    nameof(old_string));
            }

            var index = text.IndexOf(old_string, StringComparison.Ordinal);
            var updated = replace_all
                ? text.Replace(old_string, new_string, StringComparison.Ordinal)
                : string.Concat(text.AsSpan(0, index), new_string, text.AsSpan(index + old_string.Length));
            File.WriteAllText(resolved, updated);
            return string.Create(CultureInfo.InvariantCulture, $"Edited {path} ({count} replacement(s))");
        }

        return Tool(Edit, "edit_file",
            "Replace an exact substring in a workspace file. old_string must occur exactly once (unless replace_all is true).");
    }

    private static AITool ListFilesTool(string root)
    {
        string List([Description("Relative directory to list (default: workspace root)")] string path = ".")
        {
            var rel = string.IsNullOrEmpty(path) ? "." : path;
            var start = ResolveWorkspacePath(root, rel);
            var entries = new List<string>();
            var examined = 0;

            void Walk(string dir)
            {
                // Ordinal-sorted so the walk (and therefore the ListCap truncation) is deterministic,
                // matching Go's lexical filepath.WalkDir order.
                string[] children;
                try
                {
                    children = Directory.GetFileSystemEntries(dir);
                }
                catch (Exception e) when (e is IOException or UnauthorizedAccessException)
                {
                    return; // skip unreadable dirs, keep walking (Go returns nil on walk errors)
                }

                Array.Sort(children, StringComparer.Ordinal);
                foreach (var child in children)
                {
                    if (++examined > ListWalkBudget || entries.Count >= ListCap)
                    {
                        return;
                    }

                    var isDir = Directory.Exists(child);
                    if (isDir && SkipDirs.Contains(Path.GetFileName(child)))
                    {
                        continue;
                    }

                    entries.Add(Relative(root, child) + (isDir ? "/" : string.Empty));
                    if (isDir)
                    {
                        Walk(child);
                    }
                }
            }

            if (!Directory.Exists(start))
            {
                throw new DirectoryNotFoundException($"list {rel}: not a directory");
            }

            Walk(start);
            if (entries.Count == 0)
            {
                return $"(empty: {rel})";
            }

            entries.Sort(StringComparer.Ordinal);
            var output = string.Join("\n", entries);
            return entries.Count >= ListCap
                ? output + string.Create(CultureInfo.InvariantCulture, $"\n... (truncated at {ListCap} entries)")
                : output;
        }

        return Tool(List, "list_files",
            "List files and directories under a workspace path (recursive, skips .git/node_modules/target). Relative paths.");
    }

    private static AITool GrepTool(string root)
    {
        string Grep(
            [Description("Regular expression to search for")] string pattern,
            [Description("Relative directory to search (default: workspace root)")] string path = ".")
        {
            Regex re;
            try
            {
                // A match timeout bounds catastrophic backtracking — .NET's engine backtracks where
                // Go's RE2 does not, so this keeps a hostile pattern from wedging the turn.
                re = new Regex(pattern, RegexOptions.CultureInvariant, TimeSpan.FromSeconds(2));
            }
            catch (ArgumentException e)
            {
                throw new ArgumentException($"invalid regexp: {e.Message}", nameof(pattern));
            }

            var rel = string.IsNullOrEmpty(path) ? "." : path;
            var start = ResolveWorkspacePath(root, rel);
            var matches = new List<string>();
            var examined = 0;

            void Walk(string dir)
            {
                string[] children;
                try
                {
                    children = Directory.GetFileSystemEntries(dir);
                }
                catch (Exception e) when (e is IOException or UnauthorizedAccessException)
                {
                    return;
                }

                Array.Sort(children, StringComparer.Ordinal);
                foreach (var child in children)
                {
                    if (matches.Count >= GrepMatchCap || examined > ListWalkBudget)
                    {
                        return;
                    }

                    if (Directory.Exists(child))
                    {
                        if (!SkipDirs.Contains(Path.GetFileName(child)))
                        {
                            Walk(child);
                        }

                        continue;
                    }

                    examined++;
                    string content;
                    try
                    {
                        content = File.ReadAllText(child);
                    }
                    catch (Exception e) when (e is IOException or UnauthorizedAccessException)
                    {
                        continue; // unreadable file — skip it, like Go
                    }

                    var relPath = Relative(root, child);
                    var lines = content.Split('\n');
                    for (var i = 0; i < lines.Length; i++)
                    {
                        if (!re.IsMatch(lines[i]))
                        {
                            continue;
                        }

                        matches.Add(string.Create(CultureInfo.InvariantCulture, $"{relPath}:{i + 1}:{lines[i]}"));
                        if (matches.Count >= GrepMatchCap)
                        {
                            break;
                        }
                    }
                }
            }

            if (Directory.Exists(start))
            {
                Walk(start);
            }

            if (matches.Count == 0)
            {
                return $"(no matches for \"{pattern}\")";
            }

            var output = string.Join("\n", matches);
            return matches.Count >= GrepMatchCap
                ? output + string.Create(CultureInfo.InvariantCulture, $"\n... (truncated at {GrepMatchCap} matches)")
                : output;
        }

        return Tool(Grep, "grep",
            "Search workspace file contents with a regular expression. Returns path:line:text matches (skips .git/node_modules/target).");
    }

    private static AITool BashTool(string root)
    {
        async Task<string> Bash(
            [Description("The shell command to run")] string command,
            [Description("Optional: max seconds before the command is killed (default 120)")] int timeout = BashDefaultTimeoutSeconds,
            CancellationToken cancellationToken = default)
        {
            if (CatastrophicBash.IsMatch(command))
            {
                return $"BLOCKED: refused to run a catastrophic command (e.g. `rm -rf /`, fork bomb, mkfs): {command}";
            }

            var seconds = timeout > 0 ? timeout : BashDefaultTimeoutSeconds;
            using var process = new Process
            {
                StartInfo = new ProcessStartInfo("sh")
                {
                    ArgumentList = { "-c", command },
                    WorkingDirectory = root,
                    RedirectStandardOutput = true,
                    RedirectStandardError = true,
                    UseShellExecute = false,
                },
            };
            process.Start();

            // Drain both pipes concurrently — a full pipe buffer would otherwise deadlock the wait.
            var stdout = process.StandardOutput.ReadToEndAsync(cancellationToken);
            var stderr = process.StandardError.ReadToEndAsync(cancellationToken);

            using var timer = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
            timer.CancelAfter(TimeSpan.FromSeconds(seconds));
            var killed = false;
            try
            {
                await process.WaitForExitAsync(timer.Token).ConfigureAwait(false);
            }
            catch (OperationCanceledException)
            {
                killed = true;
                try
                {
                    process.Kill(entireProcessTree: true);
                }
                catch (InvalidOperationException)
                {
                    // already exited between the timeout and the kill — nothing to do
                }
            }

            var output = Truncate(await Safe(stdout).ConfigureAwait(false) + await Safe(stderr).ConfigureAwait(false));
            return killed
                ? string.Create(CultureInfo.InvariantCulture, $"exit: killed (timeout after {seconds}s)\n{output}")
                : string.Create(CultureInfo.InvariantCulture, $"exit: {process.ExitCode}\n{output}");
        }

        return Tool(Bash, "bash",
            "Run a shell command (sh -c) with the workspace as the working directory. Returns exit code, stdout, stderr.");
    }

    /// <summary>Pipe reads can fault when the process is killed — a partial/empty read is fine here.</summary>
    private static async Task<string> Safe(Task<string> read)
    {
        try
        {
            return await read.ConfigureAwait(false);
        }
        catch (Exception e) when (e is IOException or OperationCanceledException or ObjectDisposedException)
        {
            return string.Empty;
        }
    }

    private static string Truncate(string output) =>
        Encoding.UTF8.GetByteCount(output) > BashOutputCap
            ? Encoding.UTF8.GetString(Encoding.UTF8.GetBytes(output), 0, BashOutputCap) + "\n... (output truncated)"
            : output;

    /// <summary>Non-overlapping occurrence count, matching Go's <c>strings.Count</c>.</summary>
    private static int CountOccurrences(string text, string needle)
    {
        var count = 0;
        for (var i = text.IndexOf(needle, StringComparison.Ordinal); i >= 0; i = text.IndexOf(needle, i + needle.Length, StringComparison.Ordinal))
        {
            count++;
        }

        return count;
    }

    private static AITool Tool(Delegate fn, string name, string description) =>
        (AITool)AIFunctionFactory.Create(fn, new AIFunctionFactoryOptions { Name = name, Description = description });
}
