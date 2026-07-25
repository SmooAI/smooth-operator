/**
 * Coding toolset (th-82ad57) — the file-editing tools the LocalServer agent needs to
 * actually do work. Without these, `serveLocal` spins up a chat-only agent that replies
 * "I don't have file editing tools" and scores structural 0% on the parity bench.
 *
 * A faithful port of `go/server/coding_tools.go` (itself mirroring the Rust daemon's
 * `crates/smooth-tools`): read_file, write_file, edit_file, list_files, grep, bash — all
 * confined to a single workspace root. Every filesystem path the model supplies is routed
 * through {@link resolveWorkspacePath} so a prompt-injected agent can't read or write
 * outside the workspace (a trust boundary — not simplified away).
 *
 * ponytail: bash here runs unsandboxed rooted at the workspace (the Node host has no
 * kernel sandbox), matching the Go host. Acceptable for the single-trusted-user
 * loopback/bench flavor. Upgrade path: wrap a SandboxedCommand equivalent if this ever
 * serves untrusted callers.
 */
import { exec } from 'node:child_process';
import * as fs from 'node:fs/promises';
import * as path from 'node:path';

import type { Tool } from '@smooai/smooth-operator-core';

const READ_DEFAULT_LIMIT = 2000; // read_file: max lines returned when no limit given
const LIST_CAP = 200; // list_files: max entries returned
const LIST_WALK_BUDGET = 50_000; // list_files / grep: max entries examined before stopping
const GREP_MATCH_CAP = 200; // grep: max matching lines returned
const BASH_OUTPUT_CAP = 50_000; // bash: max bytes returned
const BASH_DEFAULT_KILL = 120; // bash: default timeout (seconds) if none given

/** Directory names never walked by list_files / grep. */
const SKIP_DIRS = new Set(['.git', 'node_modules', 'target']);

/**
 * Confines `rel` to `base` lexically (no symlink following, no existence requirement).
 * Accepts a relative path (joined onto base) or an absolute path that lexically resolves
 * inside base. Rejects empty paths and any path that escapes base after collapsing
 * "." / "..". Mirrors Go's `resolveWorkspacePath` / the Rust `resolve_workspace_path`.
 *
 * Exported for tests — this is the trust boundary, so it is tested directly.
 */
export function resolveWorkspacePath(base: string, rel: string): string {
    if (!rel) throw new Error('empty path');
    const baseNorm = path.resolve(base);
    // `path.resolve` collapses "..", so a contained path is baseNorm itself or is prefixed
    // by baseNorm + separator.
    const joined = path.isAbsolute(rel) ? path.normalize(rel) : path.resolve(baseNorm, rel);
    if (joined !== baseNorm && !joined.startsWith(baseNorm + path.sep)) {
        throw new Error(`path "${rel}" is outside the workspace (resolved to ${joined}, not under ${baseNorm})`);
    }
    return joined;
}

/** Pulls a required string argument out of a tool-call args map. */
function reqStr(args: Record<string, unknown>, key: string): string {
    if (!(key in args)) throw new Error(`missing required argument "${key}"`);
    const v = args[key];
    if (typeof v !== 'string') throw new Error(`argument "${key}" must be a string`);
    return v;
}

/** Pulls an optional positive integer argument (JSON numbers arrive as `number`). */
function optInt(args: Record<string, unknown>, key: string): number | undefined {
    const v = args[key];
    return typeof v === 'number' && Number.isFinite(v) ? Math.trunc(v) : undefined;
}

/** One walk entry: workspace-relative path + whether it is a directory. */
interface WalkEntry {
    rel: string;
    isDir: boolean;
}

/**
 * Depth-first walk of `root`, yielding workspace-relative paths, skipping {@link SKIP_DIRS}
 * and unreadable entries. Stops once `stop(entries)` returns true or the walk budget is
 * spent — the Go `filepath.SkipAll` equivalent.
 */
async function walk(workspace: string, root: string, stop: (entries: WalkEntry[]) => boolean): Promise<WalkEntry[]> {
    const entries: WalkEntry[] = [];
    const stack: string[] = [root];
    let examined = 0;
    while (stack.length > 0) {
        const dir = stack.pop()!;
        let dirents;
        try {
            dirents = await fs.readdir(dir, { withFileTypes: true });
        } catch {
            continue; // skip unreadable directories, keep walking
        }
        for (const d of dirents) {
            examined++;
            if (examined > LIST_WALK_BUDGET || stop(entries)) return entries;
            const isDir = d.isDirectory();
            if (isDir && SKIP_DIRS.has(d.name)) continue;
            const abs = path.join(dir, d.name);
            entries.push({ rel: path.relative(workspace, abs), isDir });
            if (isDir) stack.push(abs);
        }
    }
    return entries;
}

function readFileTool(workspace: string): Tool {
    return {
        name: 'read_file',
        description: 'Read a UTF-8 text file within the workspace. Returns line-numbered content; supports an optional line window.',
        parameters: {
            type: 'object',
            properties: {
                path: { type: 'string', description: 'Relative path within the workspace' },
                offset: { type: 'integer', description: '1-based start line (default: 1)' },
                limit: { type: 'integer', description: 'Max lines to return (default: 2000)' },
            },
            required: ['path'],
        },
        async execute(args) {
            const rel = reqStr(args, 'path');
            const file = resolveWorkspacePath(workspace, rel);
            let data: string;
            try {
                data = await fs.readFile(file, 'utf8');
            } catch (err) {
                throw new Error(`read ${rel}: ${(err as Error).message}`);
            }
            const offsetArg = optInt(args, 'offset');
            const offset = offsetArg !== undefined && offsetArg > 0 ? offsetArg : 1;
            const limitArg = optInt(args, 'limit');
            const limit = limitArg !== undefined && limitArg > 0 ? limitArg : READ_DEFAULT_LIMIT;
            const lines = data.split('\n');
            const out: string[] = [];
            for (let i = offset - 1; i < lines.length && out.length < limit; i++) {
                out.push(`${String(i + 1).padStart(6, ' ')}\t${lines[i]}`);
            }
            if (out.length === 0) return `(no lines: file has ${lines.length} line(s), offset ${offset})`;
            return `${out.join('\n')}\n`;
        },
    };
}

function writeFileTool(workspace: string): Tool {
    return {
        name: 'write_file',
        description: 'Create or overwrite a file in the workspace with the given content (parent dirs are created).',
        parameters: {
            type: 'object',
            properties: {
                path: { type: 'string', description: 'Relative path within the workspace' },
                content: { type: 'string', description: 'Full file content to write' },
            },
            required: ['path', 'content'],
        },
        async execute(args) {
            const rel = reqStr(args, 'path');
            const content = reqStr(args, 'content');
            const file = resolveWorkspacePath(workspace, rel);
            try {
                await fs.mkdir(path.dirname(file), { recursive: true });
            } catch (err) {
                throw new Error(`create parent dirs for ${rel}: ${(err as Error).message}`);
            }
            try {
                await fs.writeFile(file, content, 'utf8');
            } catch (err) {
                throw new Error(`write ${rel}: ${(err as Error).message}`);
            }
            return `Wrote ${Buffer.byteLength(content)} bytes to ${rel}`;
        },
    };
}

function editFileTool(workspace: string): Tool {
    return {
        name: 'edit_file',
        description: 'Replace an exact substring in a workspace file. old_string must occur exactly once (unless replace_all is true).',
        parameters: {
            type: 'object',
            properties: {
                path: { type: 'string', description: 'Relative path within the workspace' },
                old_string: { type: 'string', description: 'Exact text to replace' },
                new_string: { type: 'string', description: 'Replacement text' },
                replace_all: { type: 'boolean', description: 'Replace every occurrence (default: false, requires a unique match)' },
            },
            required: ['path', 'old_string', 'new_string'],
        },
        async execute(args) {
            const rel = reqStr(args, 'path');
            const oldStr = reqStr(args, 'old_string');
            const newStr = reqStr(args, 'new_string');
            const file = resolveWorkspacePath(workspace, rel);
            let text: string;
            try {
                text = await fs.readFile(file, 'utf8');
            } catch (err) {
                throw new Error(`read ${rel}: ${(err as Error).message}`);
            }
            const count = oldStr === '' ? 0 : text.split(oldStr).length - 1;
            if (count === 0) throw new Error(`old_string not found in ${rel}`);
            const replaceAll = args.replace_all === true;
            if (!replaceAll && count > 1) {
                throw new Error(`old_string occurs ${count} times in ${rel}; pass replace_all or supply a unique string`);
            }
            const updated = replaceAll ? text.split(oldStr).join(newStr) : text.replace(oldStr, newStr);
            try {
                await fs.writeFile(file, updated, 'utf8');
            } catch (err) {
                throw new Error(`write ${rel}: ${(err as Error).message}`);
            }
            return `Edited ${rel} (${count} replacement(s))`;
        },
    };
}

function listFilesTool(workspace: string): Tool {
    return {
        name: 'list_files',
        description: 'List files and directories under a workspace path (recursive, skips .git/node_modules/target). Relative paths.',
        parameters: {
            type: 'object',
            properties: {
                path: { type: 'string', description: 'Relative directory to list (default: workspace root)' },
            },
        },
        async execute(args) {
            const rel = typeof args.path === 'string' && args.path !== '' ? args.path : '.';
            const root = resolveWorkspacePath(workspace, rel);
            const found = await walk(workspace, root, (e) => e.length >= LIST_CAP);
            if (found.length === 0) return `(empty: ${rel})`;
            const names = found.map((e) => (e.isDir ? `${e.rel}/` : e.rel)).sort();
            let out = names.join('\n');
            if (names.length >= LIST_CAP) out += `\n... (truncated at ${LIST_CAP} entries)`;
            return out;
        },
    };
}

function grepTool(workspace: string): Tool {
    return {
        name: 'grep',
        description: 'Search workspace file contents with a regular expression. Returns path:line:text matches (skips .git/node_modules/target).',
        parameters: {
            type: 'object',
            properties: {
                pattern: { type: 'string', description: 'Regular expression to search for' },
                path: { type: 'string', description: 'Relative directory to search (default: workspace root)' },
            },
            required: ['pattern'],
        },
        async execute(args) {
            const pattern = reqStr(args, 'pattern');
            let re: RegExp;
            try {
                re = new RegExp(pattern);
            } catch (err) {
                throw new Error(`invalid regexp: ${(err as Error).message}`);
            }
            const rel = typeof args.path === 'string' && args.path !== '' ? args.path : '.';
            const root = resolveWorkspacePath(workspace, rel);
            const files = await walk(workspace, root, () => false);
            const matches: string[] = [];
            for (const entry of files) {
                if (entry.isDir) continue;
                if (matches.length >= GREP_MATCH_CAP) break;
                let data: string;
                try {
                    data = await fs.readFile(path.join(workspace, entry.rel), 'utf8');
                } catch {
                    continue; // unreadable / binary — skip, matching Go
                }
                const lines = data.split('\n');
                for (let i = 0; i < lines.length; i++) {
                    if (re.test(lines[i]!)) {
                        matches.push(`${entry.rel}:${i + 1}:${lines[i]}`);
                        if (matches.length >= GREP_MATCH_CAP) break;
                    }
                }
            }
            if (matches.length === 0) return `(no matches for "${pattern}")`;
            let out = matches.join('\n');
            if (matches.length >= GREP_MATCH_CAP) out += `\n... (truncated at ${GREP_MATCH_CAP} matches)`;
            return out;
        },
    };
}

/**
 * A cheap defense-in-depth deny for the handful of commands that are unrecoverable
 * regardless of workspace confinement (the Node host has no kernel sandbox). Mirrors the
 * Go/Rust bash circuit-breaker. NOT a substitute for a real sandbox.
 */
const CATASTROPHIC_BASH = /rm\s+-\S*[rf]\S*\s+(\/|~|\$HOME)(\s|\/|;|$)|:\s*\(\s*\)\s*\{|>\s*\/dev\/sd|mkfs/;

function bashTool(workspace: string): Tool {
    return {
        name: 'bash',
        description: 'Run a shell command (sh -c) with the workspace as the working directory. Returns exit code, stdout, stderr.',
        parameters: {
            type: 'object',
            properties: {
                command: { type: 'string', description: 'The shell command to run' },
                timeout: { type: 'integer', description: 'Optional: max seconds before the command is killed (default 120)' },
            },
            required: ['command'],
        },
        async execute(args) {
            const command = reqStr(args, 'command');
            if (CATASTROPHIC_BASH.test(command)) {
                return `BLOCKED: refused to run a catastrophic command (e.g. \`rm -rf /\`, fork bomb, mkfs): ${command}`;
            }
            const timeoutArg = optInt(args, 'timeout');
            const seconds = timeoutArg !== undefined && timeoutArg > 0 ? timeoutArg : BASH_DEFAULT_KILL;
            const { code, output, timedOut } = await new Promise<{ code: number; output: string; timedOut: boolean }>((resolve) => {
                exec(
                    command,
                    { cwd: workspace, shell: '/bin/sh', timeout: seconds * 1000, maxBuffer: 10 * 1024 * 1024, encoding: 'utf8' },
                    (err, stdout, stderr) => {
                        const combined = `${stdout}${stderr}`;
                        const e = err as (Error & { code?: number | string; killed?: boolean }) | null;
                        resolve({
                            code: e ? (typeof e.code === 'number' ? e.code : -1) : 0,
                            output: combined,
                            timedOut: e?.killed === true,
                        });
                    },
                );
            });
            const truncated = Buffer.byteLength(output) > BASH_OUTPUT_CAP ? `${output.slice(0, BASH_OUTPUT_CAP)}\n... (output truncated)` : output;
            if (timedOut) return `exit: killed (timeout after ${seconds}s)\n${truncated}`;
            return `exit: ${code}\n${truncated}`;
        },
    };
}

/**
 * Builds the workspace-confined coding toolset for a LocalServer agent. Pass the result as
 * `serveLocal({ tools })`. `workspace` is the root every file operation is confined to and
 * the working directory bash commands start in (the serve binary launches with
 * cwd == workspace, so "." is the natural argument).
 */
export function codingTools(workspace: string): Tool[] {
    const root = path.resolve(workspace);
    return [readFileTool(root), writeFileTool(root), editFileTool(root), listFilesTool(root), grepTool(root), bashTool(root)];
}
