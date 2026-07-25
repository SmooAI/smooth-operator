/**
 * Coding-toolset tests (th-82ad57) — the TS mirror of `go/server/coding_tools_test.go`.
 * Covers the six-tool set, each tool's happy + error path, and (the security-critical bit)
 * that the workspace path guard rejects escapes.
 */
import { mkdtemp, mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { afterEach, describe, expect, it } from 'vitest';

import { codingTools, resolveWorkspacePath } from '../src/codingTools.js';

const created: string[] = [];

async function tempWorkspace(): Promise<string> {
    // realpath-ish: macOS /var → /private/var, and the guard compares resolved paths, so the
    // workspace root must be the same string the tools resolve to.
    const dir = await mkdtemp(path.join(await realTmp(), 'coding-tools-'));
    created.push(dir);
    return dir;
}

async function realTmp(): Promise<string> {
    const { realpath } = await import('node:fs/promises');
    return realpath(tmpdir());
}

afterEach(async () => {
    await Promise.all(created.splice(0).map((d) => rm(d, { recursive: true, force: true })));
});

function toolByName(workspace: string, name: string) {
    const tool = codingTools(workspace).find((t) => t.name === name);
    if (!tool) throw new Error(`tool "${name}" not found`);
    return tool;
}

describe('codingTools', () => {
    it('exposes exactly the six parity tools', async () => {
        const tools = codingTools(await tempWorkspace());
        expect(tools.map((t) => t.name).sort()).toEqual(['bash', 'edit_file', 'grep', 'list_files', 'read_file', 'write_file']);
    });

    it('writes then reads a file', async () => {
        const ws = await tempWorkspace();
        const res = await toolByName(ws, 'write_file').execute({ path: 'hello.txt', content: 'WORLD' });
        expect(res).toContain('Wrote');
        expect(await readFile(path.join(ws, 'hello.txt'), 'utf8')).toBe('WORLD');

        const rd = await toolByName(ws, 'read_file').execute({ path: 'hello.txt' });
        expect(rd).toContain('WORLD');
        expect(rd).toContain('     1\t'); // line-numbered
    });

    it('honours the read_file line window', async () => {
        const ws = await tempWorkspace();
        await writeFile(path.join(ws, 'many.txt'), 'a\nb\nc\nd\n');
        const rd = await toolByName(ws, 'read_file').execute({ path: 'many.txt', offset: 2, limit: 2 });
        expect(rd).toContain('b');
        expect(rd).toContain('c');
        expect(rd).not.toContain('\ta\n');
        expect(rd).not.toContain('d');
    });

    it('read_file errors on a missing file', async () => {
        const ws = await tempWorkspace();
        await expect(toolByName(ws, 'read_file').execute({ path: 'nope.txt' })).rejects.toThrow(/read nope.txt/);
    });

    it('write_file creates parent dirs', async () => {
        const ws = await tempWorkspace();
        await toolByName(ws, 'write_file').execute({ path: 'a/b/c.txt', content: 'deep' });
        expect(await readFile(path.join(ws, 'a/b/c.txt'), 'utf8')).toBe('deep');
    });

    it('edits a file, refusing non-unique matches without replace_all', async () => {
        const ws = await tempWorkspace();
        await writeFile(path.join(ws, 'f.txt'), 'foo bar foo');
        const edit = toolByName(ws, 'edit_file');

        await expect(edit.execute({ path: 'f.txt', old_string: 'foo', new_string: 'X' })).rejects.toThrow(/occurs 2 times/);

        await edit.execute({ path: 'f.txt', old_string: 'foo', new_string: 'X', replace_all: true });
        expect(await readFile(path.join(ws, 'f.txt'), 'utf8')).toBe('X bar X');

        await expect(edit.execute({ path: 'f.txt', old_string: 'nope', new_string: 'Y' })).rejects.toThrow(/not found/);
    });

    it('lists and greps, skipping node_modules', async () => {
        const ws = await tempWorkspace();
        await writeFile(path.join(ws, 'one.txt'), 'needle here\nplain');
        await mkdir(path.join(ws, 'node_modules', 'junk'), { recursive: true });
        await writeFile(path.join(ws, 'node_modules', 'junk', 'x.txt'), 'needle');

        const ls = await toolByName(ws, 'list_files').execute({});
        expect(ls).toContain('one.txt');
        expect(ls).not.toContain('node_modules');

        const gr = await toolByName(ws, 'grep').execute({ pattern: 'needle' });
        expect(gr).toContain('one.txt:1:');
        expect(gr).not.toContain('node_modules');
    });

    it('grep reports no matches and rejects an invalid regexp', async () => {
        const ws = await tempWorkspace();
        await writeFile(path.join(ws, 'one.txt'), 'plain');
        expect(await toolByName(ws, 'grep').execute({ pattern: 'zzz' })).toContain('(no matches');
        await expect(toolByName(ws, 'grep').execute({ pattern: '([' })).rejects.toThrow(/invalid regexp/);
    });

    it('runs bash in the workspace', async () => {
        const ws = await tempWorkspace();
        const res = await toolByName(ws, 'bash').execute({ command: 'echo hi > out.txt && cat out.txt' });
        expect(res).toContain('exit: 0');
        expect(res).toContain('hi');
        expect((await readFile(path.join(ws, 'out.txt'), 'utf8')).trim()).toBe('hi');
    });

    it('reports a non-zero bash exit code', async () => {
        const ws = await tempWorkspace();
        expect(await toolByName(ws, 'bash').execute({ command: 'exit 3' })).toContain('exit: 3');
    });

    it('kills a bash command that exceeds its timeout', async () => {
        const ws = await tempWorkspace();
        const res = await toolByName(ws, 'bash').execute({ command: 'sleep 5', timeout: 1 });
        expect(res).toContain('killed (timeout after 1s)');
    });

    it('blocks catastrophic bash commands', async () => {
        const ws = await tempWorkspace();
        expect(await toolByName(ws, 'bash').execute({ command: 'rm -rf /' })).toContain('BLOCKED');
    });

    it('requires the documented arguments', async () => {
        const ws = await tempWorkspace();
        await expect(toolByName(ws, 'read_file').execute({})).rejects.toThrow(/missing required argument "path"/);
        await expect(toolByName(ws, 'write_file').execute({ path: 'x', content: 5 })).rejects.toThrow(/must be a string/);
    });
});

describe('resolveWorkspacePath (trust boundary)', () => {
    it('rejects escapes and accepts contained paths', async () => {
        const ws = await tempWorkspace();
        expect(() => resolveWorkspacePath(ws, '/etc/passwd')).toThrow(/outside the workspace/);
        expect(() => resolveWorkspacePath(ws, '../../etc/passwd')).toThrow(/outside the workspace/);
        expect(() => resolveWorkspacePath(ws, 'sub/../../escape')).toThrow(/outside the workspace/);
        expect(() => resolveWorkspacePath(ws, `${ws}/../sibling`)).toThrow(/outside the workspace/);
        expect(() => resolveWorkspacePath(ws, '')).toThrow(/empty path/);

        expect(resolveWorkspacePath(ws, 'sub/file.txt')).toBe(path.join(ws, 'sub/file.txt'));
        expect(resolveWorkspacePath(ws, path.join(ws, 'in.txt'))).toBe(path.join(ws, 'in.txt'));
        expect(resolveWorkspacePath(ws, './nested/../ok.txt')).toBe(path.join(ws, 'ok.txt'));
    });

    it('is enforced by the tools themselves, not just the helper', async () => {
        const ws = await tempWorkspace();
        await expect(toolByName(ws, 'write_file').execute({ path: '../escape.txt', content: 'x' })).rejects.toThrow(/outside the workspace/);
        await expect(toolByName(ws, 'read_file').execute({ path: '/etc/passwd' })).rejects.toThrow(/outside the workspace/);
        await expect(toolByName(ws, 'edit_file').execute({ path: '../f.txt', old_string: 'a', new_string: 'b' })).rejects.toThrow(
            /outside the workspace/,
        );
        await expect(toolByName(ws, 'list_files').execute({ path: '..' })).rejects.toThrow(/outside the workspace/);
        await expect(toolByName(ws, 'grep').execute({ pattern: 'x', path: '/etc' })).rejects.toThrow(/outside the workspace/);
    });
});
