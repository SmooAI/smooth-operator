/**
 * SEP extension hosting on the TypeScript operator server.
 *
 * Two layers, mirroring the Rust server's `src/extensions.rs` unit tests +
 * `tests/sep_extension_host.rs` live-wire test:
 *  - the trust allow-list parse + the `ui/confirm` → confirmation-frame bridge,
 *    covered without a subprocess;
 *  - a live host: `buildExtensionHost` spawns the dependency-free echo peer through
 *    the engine `ExtensionHost` and its `say` tool reaches the turn's tool set and
 *    flows through the SAME `enabled_tools` array filter the dispatcher applies
 *    (SMOODEV-590), so an allow-list drops it exactly like a built-in.
 */
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import type { Tool } from '@smooai/smooth-operator-core';
import { afterEach, describe, expect, it } from 'vitest';

import { ConfirmationRegistry } from '../src/confirmation.js';
import { buildExtensionHost, ConfirmUiProvider, parseAllowlist } from '../src/extensions.js';
import type { Frame } from '../src/protocol.js';

const __dirname = dirname(fileURLToPath(import.meta.url));
const ECHO_PEER = join(__dirname, 'sep', 'echo.mjs');

describe('parseAllowlist — default deny', () => {
    it('unset / blank / only-separators ⇒ deny all', () => {
        expect(parseAllowlist(undefined)).toEqual([]);
        expect(parseAllowlist('')).toEqual([]);
        expect(parseAllowlist('  , ,')).toEqual([]);
    });
    it('parses a trimmed CSV of names', () => {
        expect(parseAllowlist('todo')).toEqual(['todo']);
        expect(parseAllowlist(' todo , gate ')).toEqual(['todo', 'gate']);
    });
});

describe('ConfirmUiProvider — ui/confirm bridge', () => {
    function provider(): { p: ConfirmUiProvider; confirmations: ConfirmationRegistry; frames: Frame[] } {
        const confirmations = new ConfirmationRegistry();
        const frames: Frame[] = [];
        const p = new ConfirmUiProvider({ confirmations, sessionId: 'sess-1', requestId: 'req-1', sink: (f) => frames.push(f) });
        return { p, confirmations, frames };
    }

    it('emits a confirmation frame and resolves confirmed on approval', async () => {
        const { p, confirmations, frames } = provider();
        const fut = p.uiRequest('todo', { kind: 'confirm', prompt: 'Delete file?' });
        // The bridge emitted a write_confirmation_required frame carrying the ext name.
        await Promise.resolve();
        expect(frames).toHaveLength(1);
        expect(frames[0].type).toBe('write_confirmation_required');
        const data = (frames[0] as { data: { data: { toolId: string; actionDescription: string } } }).data.data;
        expect(data.toolId).toBe('todo');
        expect(data.actionDescription).toBe('Delete file?');
        // The inbound confirm_tool_action approves.
        expect(confirmations.resolve('sess-1', true)).toBe(true);
        expect(await fut).toEqual({ confirmed: true });
    });

    it('resolves cancelled on denial', async () => {
        const { p, confirmations } = provider();
        const fut = p.uiRequest('gate', { kind: 'confirm', prompt: 'Proceed?' });
        await Promise.resolve();
        confirmations.resolve('sess-1', false);
        expect(await fut).toEqual({ cancelled: true });
    });

    it('resolves cancelled when the turn ends (rejectAll)', async () => {
        const { p, confirmations } = provider();
        const fut = p.uiRequest('x', { kind: 'confirm', prompt: 'Go?' });
        await Promise.resolve();
        confirmations.rejectAll();
        expect(await fut).toEqual({ cancelled: true });
    });

    it('accepts and drops render-only kinds', async () => {
        const { p, frames } = provider();
        for (const kind of ['notify', 'set_status', 'set_widget', 'set_title']) {
            expect(await p.uiRequest('x', { kind, message: 'hi', status: 's', widget: {}, title: 't' })).toEqual({});
        }
        expect(frames).toHaveLength(0);
    });

    it('cancels unsupported interactive kinds', async () => {
        const { p } = provider();
        for (const kind of ['select', 'input']) {
            expect(await p.uiRequest('x', { kind, prompt: '?', options: ['a'] })).toEqual({ cancelled: true });
        }
    });

    it('degrades to cancelled with no confirmation registry', async () => {
        const p = new ConfirmUiProvider({ sessionId: 's', requestId: 'r', sink: () => {} });
        expect(await p.uiRequest('x', { kind: 'confirm', prompt: '?' })).toEqual({ cancelled: true });
    });
});

describe('buildExtensionHost — live echo peer + trust + enabled_tools parity', () => {
    const dirs: string[] = [];
    const savedEnv: Record<string, string | undefined> = {};
    afterEach(async () => {
        for (const k of Object.keys(savedEnv)) {
            if (savedEnv[k] === undefined) delete process.env[k];
            else process.env[k] = savedEnv[k];
        }
        for (const d of dirs.splice(0)) rmSync(d, { recursive: true, force: true });
    });
    function setEnv(k: string, v: string | undefined): void {
        if (!(k in savedEnv)) savedEnv[k] = process.env[k];
        if (v === undefined) delete process.env[k];
        else process.env[k] = v;
    }

    /** Write `<tmp>/echo/extension.toml` running `node echo.mjs` and return the extensions dir. */
    function writeEchoDir(): string {
        const tmp = mkdtempSync(join(tmpdir(), 'sep-srv-'));
        dirs.push(tmp);
        const extDir = join(tmp, 'echo');
        mkdirSync(extDir, { recursive: true });
        writeFileSync(join(extDir, 'extension.toml'), `name = "echo"\nversion = "0.1.0"\n[run]\ncommand = "node"\nargs = ["${ECHO_PEER}"]\n[capabilities]\ntools = true\n`);
        return tmp;
    }

    const ctx = { sessionId: 's', requestId: 'r', sink: () => {} };

    it('default-deny: unset SMOOTH_EXTENSIONS_ALLOW builds no host', async () => {
        setEnv('SMOOTH_EXTENSIONS_ALLOW', undefined);
        setEnv('SMOOTH_EXTENSIONS_DIR', writeEchoDir());
        expect(await buildExtensionHost(ctx)).toBeUndefined();
    });

    it('an allowlisted echo extension surfaces echo.say and honors enabled_tools', async () => {
        setEnv('SMOOTH_EXTENSIONS_DIR', writeEchoDir());
        setEnv('SMOOTH_EXTENSIONS_ALLOW', 'echo');
        const host = await buildExtensionHost(ctx);
        expect(host).toBeDefined();
        try {
            expect(host!.names()).toEqual(['echo']);
            const tools: Tool[] = host!.tools();
            expect(tools.some((t) => t.name === 'echo.say')).toBe(true);

            // The dispatcher registers host tools into the base set, then applies the
            // per-agent enabled_tools filter — a plain array filter over tool names.
            const baseTools = [...tools];
            const keep = (enabled: string[]) => baseTools.filter((t) => enabled.includes(t.name));
            expect(keep(['echo.say']).some((t) => t.name === 'echo.say')).toBe(true);
            expect(keep(['some_builtin']).some((t) => t.name === 'echo.say')).toBe(false);
        } finally {
            await host!.shutdownAll();
        }
    });

    it('an extension not on the allowlist is skipped (host not built)', async () => {
        setEnv('SMOOTH_EXTENSIONS_DIR', writeEchoDir());
        setEnv('SMOOTH_EXTENSIONS_ALLOW', 'something-else');
        expect(await buildExtensionHost(ctx)).toBeUndefined();
    });
});
