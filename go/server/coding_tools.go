package server

import (
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
	"time"

	core "github.com/SmooAI/smooth-operator-core/go/core"
)

// Coding toolset (th-82ad57) — the file-editing tools the LocalServer agent needs to
// actually do work. Without these, ServeLocal spins up a chat-only agent that replies
// "I don't have file editing tools" and scores structural 0% on the parity bench.
//
// Mirrors the Rust daemon's set (crates/smooth-tools): read_file, write_file, edit_file,
// list_files, grep, bash — all confined to a single workspace root. Every filesystem path
// the model supplies is routed through resolveWorkspacePath so a prompt-injected agent
// can't read or write outside the workspace (a trust boundary — not simplified away).
//
// ponytail: bash here runs unsandboxed rooted at the workspace (Go host has no kernel
// sandbox). Acceptable for the single-trusted-user loopback/bench flavor, matching the
// Rust bash tool's own Linux fallback. Upgrade path: wrap SandboxedCommand equivalent if
// this ever serves untrusted callers.

const (
	readDefaultLimit = 2000  // read_file: max lines returned when no limit given
	listCap          = 200   // list_files: max entries returned
	listWalkBudget   = 50000 // list_files: max entries examined before stopping
	grepMatchCap     = 200   // grep: max matching lines returned
	bashOutputCap    = 50000 // bash: max bytes returned per stream
	bashDefaultKill  = 120   // bash: default timeout (seconds) if none given
)

// CodingTools builds the workspace-confined coding toolset for a LocalServer agent.
// Pass the result to WithTools. workspace is the root every file operation is confined to
// and the working directory bash commands start in (the serve binary launches with
// cwd == workspace, so "." is the natural argument).
//
// This constructor is the reusable pattern the sibling language servers (ts/python/dotnet)
// should copy — same six tools, same confinement, wired via their own WithTools analogue.
func CodingTools(workspace string) []core.Tool {
	abs, err := filepath.Abs(workspace)
	if err == nil {
		workspace = abs
	}
	workspace = filepath.Clean(workspace)
	return []core.Tool{
		readFileTool(workspace),
		writeFileTool(workspace),
		editFileTool(workspace),
		listFilesTool(workspace),
		grepTool(workspace),
		bashTool(workspace),
	}
}

// resolveWorkspacePath confines rel to base lexically (no symlink following, no existence
// requirement). Accepts a relative path (joined onto base) or an absolute path that
// lexically resolves inside base. Rejects empty paths and any path that escapes base after
// collapsing "." / "..". Mirrors the Rust resolve_workspace_path.
func resolveWorkspacePath(base, rel string) (string, error) {
	if rel == "" {
		return "", fmt.Errorf("empty path")
	}
	baseNorm := filepath.Clean(base)
	var joined string
	if filepath.IsAbs(rel) {
		joined = filepath.Clean(rel)
	} else {
		joined = filepath.Clean(filepath.Join(baseNorm, rel))
	}
	// Clean collapses "..", so a contained path is baseNorm itself or has baseNorm+sep as prefix.
	if joined != baseNorm && !strings.HasPrefix(joined, baseNorm+string(os.PathSeparator)) {
		return "", fmt.Errorf("path %q is outside the workspace (resolved to %s, not under %s)", rel, joined, baseNorm)
	}
	return joined, nil
}

// reqStr pulls a required string argument from a tool-call args map.
func reqStr(args map[string]any, key string) (string, error) {
	v, ok := args[key]
	if !ok {
		return "", fmt.Errorf("missing required argument %q", key)
	}
	s, ok := v.(string)
	if !ok {
		return "", fmt.Errorf("argument %q must be a string", key)
	}
	return s, nil
}

// optInt pulls an optional integer argument (JSON numbers decode as float64).
func optInt(args map[string]any, key string) (int, bool) {
	v, ok := args[key]
	if !ok {
		return 0, false
	}
	switch n := v.(type) {
	case float64:
		return int(n), true
	case int:
		return n, true
	}
	return 0, false
}

func readFileTool(workspace string) core.Tool {
	return core.FuncTool{
		ToolName: "read_file",
		Desc:     "Read a UTF-8 text file within the workspace. Returns line-numbered content; supports an optional line window.",
		Params: map[string]any{
			"type": "object",
			"properties": map[string]any{
				"path":   map[string]any{"type": "string", "description": "Relative path within the workspace"},
				"offset": map[string]any{"type": "integer", "description": "1-based start line (default: 1)"},
				"limit":  map[string]any{"type": "integer", "description": "Max lines to return (default: 2000)"},
			},
			"required": []any{"path"},
		},
		Fn: func(_ context.Context, args map[string]any) (string, error) {
			rel, err := reqStr(args, "path")
			if err != nil {
				return "", err
			}
			path, err := resolveWorkspacePath(workspace, rel)
			if err != nil {
				return "", err
			}
			data, err := os.ReadFile(path)
			if err != nil {
				return "", fmt.Errorf("read %s: %w", rel, err)
			}
			offset := 1
			if o, ok := optInt(args, "offset"); ok && o > 0 {
				offset = o
			}
			limit := readDefaultLimit
			if l, ok := optInt(args, "limit"); ok && l > 0 {
				limit = l
			}
			lines := strings.Split(string(data), "\n")
			var b strings.Builder
			shown := 0
			for i := offset - 1; i < len(lines) && shown < limit; i++ {
				fmt.Fprintf(&b, "%6d\t%s\n", i+1, lines[i])
				shown++
			}
			if shown == 0 {
				return fmt.Sprintf("(no lines: file has %d line(s), offset %d)", len(lines), offset), nil
			}
			return b.String(), nil
		},
	}
}

func writeFileTool(workspace string) core.Tool {
	return core.FuncTool{
		ToolName: "write_file",
		Desc:     "Create or overwrite a file in the workspace with the given content (parent dirs are created).",
		Params: map[string]any{
			"type": "object",
			"properties": map[string]any{
				"path":    map[string]any{"type": "string", "description": "Relative path within the workspace"},
				"content": map[string]any{"type": "string", "description": "Full file content to write"},
			},
			"required": []any{"path", "content"},
		},
		Fn: func(_ context.Context, args map[string]any) (string, error) {
			rel, err := reqStr(args, "path")
			if err != nil {
				return "", err
			}
			content, err := reqStr(args, "content")
			if err != nil {
				return "", err
			}
			path, err := resolveWorkspacePath(workspace, rel)
			if err != nil {
				return "", err
			}
			if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
				return "", fmt.Errorf("create parent dirs for %s: %w", rel, err)
			}
			if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
				return "", fmt.Errorf("write %s: %w", rel, err)
			}
			return fmt.Sprintf("Wrote %d bytes to %s", len(content), rel), nil
		},
	}
}

func editFileTool(workspace string) core.Tool {
	return core.FuncTool{
		ToolName: "edit_file",
		Desc:     "Replace an exact substring in a workspace file. old_string must occur exactly once (unless replace_all is true).",
		Params: map[string]any{
			"type": "object",
			"properties": map[string]any{
				"path":        map[string]any{"type": "string", "description": "Relative path within the workspace"},
				"old_string":  map[string]any{"type": "string", "description": "Exact text to replace"},
				"new_string":  map[string]any{"type": "string", "description": "Replacement text"},
				"replace_all": map[string]any{"type": "boolean", "description": "Replace every occurrence (default: false, requires a unique match)"},
			},
			"required": []any{"path", "old_string", "new_string"},
		},
		Fn: func(_ context.Context, args map[string]any) (string, error) {
			rel, err := reqStr(args, "path")
			if err != nil {
				return "", err
			}
			oldStr, err := reqStr(args, "old_string")
			if err != nil {
				return "", err
			}
			newStr, err := reqStr(args, "new_string")
			if err != nil {
				return "", err
			}
			path, err := resolveWorkspacePath(workspace, rel)
			if err != nil {
				return "", err
			}
			data, err := os.ReadFile(path)
			if err != nil {
				return "", fmt.Errorf("read %s: %w", rel, err)
			}
			text := string(data)
			count := strings.Count(text, oldStr)
			if count == 0 {
				return "", fmt.Errorf("old_string not found in %s", rel)
			}
			replaceAll, _ := args["replace_all"].(bool)
			if !replaceAll && count > 1 {
				return "", fmt.Errorf("old_string occurs %d times in %s; pass replace_all or supply a unique string", count, rel)
			}
			var updated string
			if replaceAll {
				updated = strings.ReplaceAll(text, oldStr, newStr)
			} else {
				updated = strings.Replace(text, oldStr, newStr, 1)
			}
			if err := os.WriteFile(path, []byte(updated), 0o644); err != nil {
				return "", fmt.Errorf("write %s: %w", rel, err)
			}
			return fmt.Sprintf("Edited %s (%d replacement(s))", rel, count), nil
		},
	}
}

func listFilesTool(workspace string) core.Tool {
	return core.FuncTool{
		ToolName: "list_files",
		Desc:     "List files and directories under a workspace path (recursive, skips .git/node_modules/target). Relative paths.",
		Params: map[string]any{
			"type": "object",
			"properties": map[string]any{
				"path": map[string]any{"type": "string", "description": "Relative directory to list (default: workspace root)"},
			},
		},
		Fn: func(_ context.Context, args map[string]any) (string, error) {
			rel := "."
			if r, ok := args["path"].(string); ok && r != "" {
				rel = r
			}
			root, err := resolveWorkspacePath(workspace, rel)
			if err != nil {
				return "", err
			}
			var entries []string
			examined := 0
			walkErr := filepath.WalkDir(root, func(p string, d os.DirEntry, err error) error {
				if err != nil {
					return nil // skip unreadable entries, keep walking
				}
				examined++
				if examined > listWalkBudget || len(entries) >= listCap {
					return filepath.SkipAll
				}
				name := d.Name()
				if d.IsDir() && (name == ".git" || name == "node_modules" || name == "target") {
					return filepath.SkipDir
				}
				if p == root {
					return nil
				}
				relPath, rerr := filepath.Rel(workspace, p)
				if rerr != nil {
					relPath = p
				}
				if d.IsDir() {
					relPath += "/"
				}
				entries = append(entries, relPath)
				return nil
			})
			if walkErr != nil {
				return "", fmt.Errorf("list %s: %w", rel, walkErr)
			}
			if len(entries) == 0 {
				return fmt.Sprintf("(empty: %s)", rel), nil
			}
			sort.Strings(entries)
			out := strings.Join(entries, "\n")
			if len(entries) >= listCap {
				out += fmt.Sprintf("\n... (truncated at %d entries)", listCap)
			}
			return out, nil
		},
	}
}

func grepTool(workspace string) core.Tool {
	return core.FuncTool{
		ToolName: "grep",
		Desc:     "Search workspace file contents with a Go regular expression. Returns path:line:text matches (skips .git/node_modules/target).",
		Params: map[string]any{
			"type": "object",
			"properties": map[string]any{
				"pattern": map[string]any{"type": "string", "description": "Go regexp to search for"},
				"path":    map[string]any{"type": "string", "description": "Relative directory to search (default: workspace root)"},
			},
			"required": []any{"pattern"},
		},
		Fn: func(_ context.Context, args map[string]any) (string, error) {
			pattern, err := reqStr(args, "pattern")
			if err != nil {
				return "", err
			}
			re, err := regexp.Compile(pattern)
			if err != nil {
				return "", fmt.Errorf("invalid regexp: %w", err)
			}
			rel := "."
			if r, ok := args["path"].(string); ok && r != "" {
				rel = r
			}
			root, err := resolveWorkspacePath(workspace, rel)
			if err != nil {
				return "", err
			}
			var matches []string
			examined := 0
			_ = filepath.WalkDir(root, func(p string, d os.DirEntry, err error) error {
				if err != nil {
					return nil
				}
				if d.IsDir() {
					name := d.Name()
					if name == ".git" || name == "node_modules" || name == "target" {
						return filepath.SkipDir
					}
					return nil
				}
				examined++
				if examined > listWalkBudget || len(matches) >= grepMatchCap {
					return filepath.SkipAll
				}
				data, rerr := os.ReadFile(p)
				if rerr != nil {
					return nil
				}
				relPath, _ := filepath.Rel(workspace, p)
				for i, line := range strings.Split(string(data), "\n") {
					if re.MatchString(line) {
						matches = append(matches, fmt.Sprintf("%s:%d:%s", relPath, i+1, line))
						if len(matches) >= grepMatchCap {
							break
						}
					}
				}
				return nil
			})
			if len(matches) == 0 {
				return fmt.Sprintf("(no matches for %q)", pattern), nil
			}
			out := strings.Join(matches, "\n")
			if len(matches) >= grepMatchCap {
				out += fmt.Sprintf("\n... (truncated at %d matches)", grepMatchCap)
			}
			return out, nil
		},
	}
}

// catastrophicBash is a cheap defense-in-depth deny for the handful of commands that are
// unrecoverable regardless of workspace confinement (the Go host has no kernel sandbox).
// Mirrors the Rust bash tool's circuit-breaker check. NOT a substitute for a real sandbox.
var catastrophicBash = regexp.MustCompile(`rm\s+-\S*[rf]\S*\s+(/|~|\$HOME)(\s|/|;|$)|:\s*\(\s*\)\s*\{|>\s*/dev/sd|mkfs`)

func bashTool(workspace string) core.Tool {
	return core.FuncTool{
		ToolName: "bash",
		Desc:     "Run a shell command (sh -c) with the workspace as the working directory. Returns exit code, stdout, stderr.",
		Params: map[string]any{
			"type": "object",
			"properties": map[string]any{
				"command": map[string]any{"type": "string", "description": "The shell command to run"},
				"timeout": map[string]any{"type": "integer", "description": "Optional: max seconds before the command is killed (default 120)"},
			},
			"required": []any{"command"},
		},
		Fn: func(ctx context.Context, args map[string]any) (string, error) {
			command, err := reqStr(args, "command")
			if err != nil {
				return "", err
			}
			if catastrophicBash.MatchString(command) {
				return fmt.Sprintf("BLOCKED: refused to run a catastrophic command (e.g. `rm -rf /`, fork bomb, mkfs): %s", command), nil
			}
			timeout := time.Duration(bashDefaultKill) * time.Second
			if t, ok := optInt(args, "timeout"); ok && t > 0 {
				timeout = time.Duration(t) * time.Second
			}
			cctx, cancel := context.WithTimeout(ctx, timeout)
			defer cancel()

			cmd := exec.CommandContext(cctx, "sh", "-c", command)
			cmd.Dir = workspace
			out, runErr := cmd.CombinedOutput()
			if len(out) > bashOutputCap {
				out = append(out[:bashOutputCap], []byte("\n... (output truncated)")...)
			}
			exitCode := 0
			if runErr != nil {
				if ee, ok := runErr.(*exec.ExitError); ok {
					exitCode = ee.ExitCode()
				} else {
					exitCode = -1
				}
			}
			if cctx.Err() == context.DeadlineExceeded {
				return fmt.Sprintf("exit: killed (timeout after %s)\n%s", timeout, out), nil
			}
			return fmt.Sprintf("exit: %d\n%s", exitCode, out), nil
		},
	}
}
