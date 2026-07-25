package server

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"

	core "github.com/SmooAI/smooth-operator-core/go/core"
)

// toolByName pulls a tool out of the CodingTools set by name (they're addressed by name at
// dispatch, so the test does too).
func toolByName(t *testing.T, tools []core.Tool, name string) core.Tool {
	t.Helper()
	for _, tl := range tools {
		if tl.Name() == name {
			return tl
		}
	}
	t.Fatalf("tool %q not found", name)
	return nil
}

func TestCodingToolsSet(t *testing.T) {
	tools := CodingTools(t.TempDir())
	want := []string{"read_file", "write_file", "edit_file", "list_files", "grep", "bash"}
	if len(tools) != len(want) {
		t.Fatalf("got %d tools, want %d", len(tools), len(want))
	}
	for _, n := range want {
		toolByName(t, tools, n) // fatals if missing
	}
}

func TestWriteThenReadFile(t *testing.T) {
	ws := t.TempDir()
	tools := CodingTools(ws)
	ctx := context.Background()

	res, err := toolByName(t, tools, "write_file").Execute(ctx, map[string]any{"path": "hello.txt", "content": "WORLD"})
	if err != nil {
		t.Fatalf("write_file: %v", err)
	}
	if !strings.Contains(res, "Wrote") {
		t.Errorf("write result = %q, want a write confirmation", res)
	}
	// The file actually landed on disk with the exact bytes.
	got, err := os.ReadFile(filepath.Join(ws, "hello.txt"))
	if err != nil {
		t.Fatalf("read back: %v", err)
	}
	if string(got) != "WORLD" {
		t.Errorf("file content = %q, want %q", got, "WORLD")
	}
	// read_file returns line-numbered content.
	rd, err := toolByName(t, tools, "read_file").Execute(ctx, map[string]any{"path": "hello.txt"})
	if err != nil {
		t.Fatalf("read_file: %v", err)
	}
	if !strings.Contains(rd, "WORLD") {
		t.Errorf("read_file = %q, want it to contain WORLD", rd)
	}
}

func TestWriteFileCreatesParentDirs(t *testing.T) {
	ws := t.TempDir()
	tools := CodingTools(ws)
	if _, err := toolByName(t, tools, "write_file").Execute(context.Background(),
		map[string]any{"path": "a/b/c.txt", "content": "deep"}); err != nil {
		t.Fatalf("write nested: %v", err)
	}
	if got, _ := os.ReadFile(filepath.Join(ws, "a/b/c.txt")); string(got) != "deep" {
		t.Errorf("nested file content = %q, want %q", got, "deep")
	}
}

func TestEditFile(t *testing.T) {
	ws := t.TempDir()
	tools := CodingTools(ws)
	ctx := context.Background()
	os.WriteFile(filepath.Join(ws, "f.txt"), []byte("foo bar foo"), 0o644)

	// Non-unique old_string without replace_all is refused.
	if _, err := toolByName(t, tools, "edit_file").Execute(ctx,
		map[string]any{"path": "f.txt", "old_string": "foo", "new_string": "X"}); err == nil {
		t.Error("expected error editing a non-unique string without replace_all")
	}
	// replace_all rewrites every occurrence.
	if _, err := toolByName(t, tools, "edit_file").Execute(ctx,
		map[string]any{"path": "f.txt", "old_string": "foo", "new_string": "X", "replace_all": true}); err != nil {
		t.Fatalf("edit replace_all: %v", err)
	}
	if got, _ := os.ReadFile(filepath.Join(ws, "f.txt")); string(got) != "X bar X" {
		t.Errorf("after edit = %q, want %q", got, "X bar X")
	}
	// Missing old_string is an error.
	if _, err := toolByName(t, tools, "edit_file").Execute(ctx,
		map[string]any{"path": "f.txt", "old_string": "nope", "new_string": "Y"}); err == nil {
		t.Error("expected error when old_string absent")
	}
}

func TestListAndGrep(t *testing.T) {
	ws := t.TempDir()
	tools := CodingTools(ws)
	ctx := context.Background()
	os.WriteFile(filepath.Join(ws, "one.txt"), []byte("needle here\nplain"), 0o644)
	os.MkdirAll(filepath.Join(ws, "node_modules", "junk"), 0o755)
	os.WriteFile(filepath.Join(ws, "node_modules", "junk", "x.txt"), []byte("needle"), 0o644)

	ls, err := toolByName(t, tools, "list_files").Execute(ctx, map[string]any{})
	if err != nil {
		t.Fatalf("list_files: %v", err)
	}
	if !strings.Contains(ls, "one.txt") {
		t.Errorf("list = %q, want one.txt", ls)
	}
	if strings.Contains(ls, "node_modules") {
		t.Errorf("list = %q, should skip node_modules", ls)
	}

	gr, err := toolByName(t, tools, "grep").Execute(ctx, map[string]any{"pattern": "needle"})
	if err != nil {
		t.Fatalf("grep: %v", err)
	}
	if !strings.Contains(gr, "one.txt:1:") {
		t.Errorf("grep = %q, want a one.txt:1 match", gr)
	}
	if strings.Contains(gr, "node_modules") {
		t.Errorf("grep = %q, should skip node_modules", gr)
	}
}

func TestBashRunsInWorkspace(t *testing.T) {
	ws := t.TempDir()
	tools := CodingTools(ws)
	res, err := toolByName(t, tools, "bash").Execute(context.Background(),
		map[string]any{"command": "echo hi > out.txt && cat out.txt"})
	if err != nil {
		t.Fatalf("bash: %v", err)
	}
	if !strings.Contains(res, "exit: 0") || !strings.Contains(res, "hi") {
		t.Errorf("bash result = %q, want exit 0 + hi", res)
	}
	if got, _ := os.ReadFile(filepath.Join(ws, "out.txt")); strings.TrimSpace(string(got)) != "hi" {
		t.Errorf("bash cwd wrong: out.txt = %q", got)
	}
}

func TestBashBlocksCatastrophic(t *testing.T) {
	tools := CodingTools(t.TempDir())
	res, err := toolByName(t, tools, "bash").Execute(context.Background(),
		map[string]any{"command": "rm -rf /"})
	if err != nil {
		t.Fatalf("bash: %v", err)
	}
	if !strings.Contains(res, "BLOCKED") {
		t.Errorf("bash result = %q, want BLOCKED for rm -rf /", res)
	}
}

func TestPathConfinement(t *testing.T) {
	ws := t.TempDir()
	// Absolute escape.
	if _, err := resolveWorkspacePath(ws, "/etc/passwd"); err == nil {
		t.Error("expected /etc/passwd to be rejected")
	}
	// Relative escape.
	if _, err := resolveWorkspacePath(ws, "../../etc/passwd"); err == nil {
		t.Error("expected ../../etc/passwd to be rejected")
	}
	// Empty.
	if _, err := resolveWorkspacePath(ws, ""); err == nil {
		t.Error("expected empty path to be rejected")
	}
	// Contained relative + absolute-within both allowed.
	if _, err := resolveWorkspacePath(ws, "sub/file.txt"); err != nil {
		t.Errorf("contained relative rejected: %v", err)
	}
	if _, err := resolveWorkspacePath(ws, filepath.Join(ws, "in.txt")); err != nil {
		t.Errorf("absolute-within-workspace rejected: %v", err)
	}
}

func TestWriteFileRejectsEscape(t *testing.T) {
	ws := t.TempDir()
	tools := CodingTools(ws)
	if _, err := toolByName(t, tools, "write_file").Execute(context.Background(),
		map[string]any{"path": "../escape.txt", "content": "x"}); err == nil {
		t.Error("write_file should reject a path escaping the workspace")
	}
}
