/**
 * The `toolHooks` server seam: a consumer-supplied {@link ToolHook} installed via
 * {@link ServerOptions.toolHooks} plumbs all the way down to every turn's engine
 * tool registry. Proven end-to-end over a real WebSocket: a redacting `postCall`
 * rewrites the tool result the client sees on the stream, and a `preCall` fires
 * for every dispatched tool.
 *
 * Mirrors the Rust server's hook-seam wiring (`LocalServerBuilder` → per-turn
 * `ToolRegistry`).
 */
import { MockLlmProvider } from '@smooai/smooth-operator-core';
import type { Tool, ToolCall, ToolHook } from '@smooai/smooth-operator-core';
import { afterEach, describe, expect, it } from 'vitest';

import { serve, type RunningServer } from '../src/server.js';
import { TestClient } from './wsClient.js';

/** A tool that returns a value containing a secret, so a redaction hook has work to do. */
function leakyTool(): Tool {
    return {
        name: 'lookup_account',
        description: 'Looks up an account (returns a secret token).',
        parameters: { type: 'object', properties: { id: { type: 'string' } }, required: [] },
        async execute() {
            return 'account ok; token=SECRET-1234';
        },
    };
}

/** Pull the tool-result chunk's `result` string out of the streamed frames. */
function toolResultText(seen: Array<Record<string, unknown>>, toolName: string): string | undefined {
    for (const frame of seen) {
        if (frame.type !== 'stream_chunk') continue;
        const state = ((frame.data as Record<string, unknown>).state as Record<string, unknown>).rawResponse as Record<string, unknown>;
        const tr = state?.toolResult as Record<string, unknown> | undefined;
        if (tr && tr.name === toolName) return tr.result as string;
    }
    return undefined;
}

describe('toolHooks server seam', () => {
    let server: RunningServer | undefined;

    afterEach(async () => {
        await server?.close();
        server = undefined;
    });

    it('a consumer postCall hook redacts the tool result the client sees, and preCall fires', async () => {
        const preCalls: ToolCall[] = [];
        const hook: ToolHook = {
            async preCall(call) {
                preCalls.push({ ...call });
            },
            async postCall(_call, result) {
                result.content = result.content.replace(/token=\S+/, 'token=[REDACTED]');
            },
        };

        const chat = new MockLlmProvider()
            .pushToolCall('call-1', 'lookup_account', JSON.stringify({ id: 'a1' }))
            .pushText('Done.');
        server = await serve({ chatClient: chat, tools: [leakyTool()], toolHooks: [hook] });
        const client = await TestClient.connect(server.url);

        client.sendAction({ action: 'create_conversation_session', requestId: 'cs' });
        const sessionId = ((await client.receive()).data as Record<string, unknown>).sessionId as string;

        client.sendAction({ action: 'send_message', requestId: 'sm', sessionId, message: 'look up account a1' });
        const { seen } = await client.receiveUntil('eventual_response');

        // preCall fired once, for the dispatched tool, with parsed args.
        expect(preCalls).toHaveLength(1);
        expect(preCalls[0].name).toBe('lookup_account');
        expect(preCalls[0].arguments).toEqual({ id: 'a1' });

        // postCall's redaction is what reached the wire — the secret never leaves the server.
        const result = toolResultText(seen, 'lookup_account');
        expect(result).toBeDefined();
        expect(result).toContain('token=[REDACTED]');
        expect(result).not.toContain('SECRET-1234');

        await client.close();
    });

    it('a preCall that throws blocks the tool — the model is told, the tool never runs', async () => {
        let ran = false;
        const tool: Tool = {
            name: 'danger',
            description: 'A tool that must be blocked.',
            parameters: { type: 'object', properties: {} },
            async execute() {
                ran = true;
                return 'should not happen';
            },
        };
        const blockHook: ToolHook = {
            async preCall(call) {
                if (call.name === 'danger') throw new Error('policy: danger blocked');
            },
        };

        const chat = new MockLlmProvider().pushToolCall('call-1', 'danger', '{}').pushText('Understood.');
        server = await serve({ chatClient: chat, tools: [tool], toolHooks: [blockHook] });
        const client = await TestClient.connect(server.url);

        client.sendAction({ action: 'create_conversation_session', requestId: 'cs' });
        const sessionId = ((await client.receive()).data as Record<string, unknown>).sessionId as string;

        client.sendAction({ action: 'send_message', requestId: 'sm', sessionId, message: 'do the dangerous thing' });
        const { seen } = await client.receiveUntil('eventual_response');

        expect(ran).toBe(false);
        const result = toolResultText(seen, 'danger');
        expect(result).toBeDefined();
        expect(result).toContain('blocked by hook');

        await client.close();
    });
});
