/**
 * Write-confirmation HITL — the pause → `confirm_tool_action` → resume path.
 *
 * Boots the real TS WS server with a confirmation-gated tool and a scripted
 * {@link MockLlmProvider} (so the turn runs offline), then drives the full seam
 * end-to-end over a real `ws` client:
 *
 *  - **Approve** → the parked tool runs; its result reaches the model (a
 *    `stream_chunk` with the tool result), the turn streams the final reply and
 *    completes with an `eventual_response`.
 *  - **Reject** → the tool is blocked; the model sees a `Denied by human` result
 *    instead, and the turn still completes (no hang).
 *
 * The TS analog of the Rust `tests/confirm_tool_action.rs` and the Python
 * `tests/test_confirm_tool_action.py`. The `confirm_tool_action` frame arrives on the
 * same connection's reader while the turn is parked — proving the turn runs as a
 * background task (not awaited inline), so the reader stays free to receive the
 * confirmation. Also covers the fail-closed validation
 * (`NO_PENDING_CONFIRMATION` / `VALIDATION_ERROR`).
 */
import { MockLlmProvider } from '@smooai/smooth-operator-core';
import type { Tool } from '@smooai/smooth-operator-core';
import { afterEach, describe, expect, it } from 'vitest';

import { serve, type RunningServer } from '../src/server.js';
import { TestClient } from './wsClient.js';

const GATED_TOOL = 'delete_record';

/** The gated write tool: returns a fixed result so an approved run is deterministic. */
function gatedTool(): Tool {
    return {
        name: GATED_TOOL,
        description: 'Delete a record by id (a state-mutating write).',
        parameters: { type: 'object', properties: { id: { type: 'string' } }, required: ['id'] },
        execute: async (): Promise<string> => 'Record 42 deleted.',
    };
}

/** Turn 1 calls the gated tool; turn 2 wraps up with a final reply. */
function scriptedMock(): MockLlmProvider {
    const mock = new MockLlmProvider();
    mock.pushToolCall('call-1', GATED_TOOL, '{"id": "42"}');
    mock.pushText('Done — record 42 was deleted.');
    return mock;
}

async function start(confirmTools: string[]): Promise<RunningServer> {
    return serve({
        chatClient: scriptedMock(),
        tools: [gatedTool()],
        confirmTools,
    });
}

/** Drive create_conversation_session and return the new session id. */
async function createSession(client: TestClient): Promise<string> {
    client.sendAction({
        action: 'create_conversation_session',
        requestId: 'r-create',
        agentId: '11111111-1111-1111-1111-111111111111',
        userName: 'Alice',
        userEmail: 'alice@example.com',
    });
    for (;;) {
        const event = await client.receive();
        if (event.type === 'immediate_response') return (event.data as { sessionId: string }).sessionId;
    }
}

/** Next protocol event, skipping non-semantic keepalive/pong frames. */
async function recv(client: TestClient): Promise<Record<string, unknown>> {
    for (;;) {
        const event = await client.receive();
        if (event.type !== 'keepalive' && event.type !== 'pong') return event;
    }
}

interface ToolResult {
    name: string;
    isError: boolean;
    result: string;
}

function toolResultOf(event: Record<string, unknown>): ToolResult | undefined {
    const state = (event.data as { state?: { rawResponse?: { toolResult?: ToolResult } } }).state;
    return state?.rawResponse?.toolResult;
}

describe('write-confirmation HITL — confirm_tool_action pause/resume', () => {
    let server: RunningServer | undefined;

    afterEach(async () => {
        await server?.close();
        server = undefined;
    });

    it('approve → the gated tool runs and the turn completes', async () => {
        server = await start([GATED_TOOL]);
        const client = await TestClient.connect(server.url);
        try {
            const sessionId = await createSession(client);

            client.sendAction({ action: 'send_message', requestId: 'r-msg', sessionId, message: 'delete record 42' });

            const ack = await recv(client);
            expect(ack.type).toBe('immediate_response');
            expect(ack.status).toBe(202);

            // The turn parks: write_confirmation_required, THEN the deferred toolCall chunk.
            const confirm = await recv(client);
            expect(confirm.type).toBe('write_confirmation_required');
            expect(confirm.requestId).toBe('r-msg');
            const prompt = confirm.data as { data: { toolId: string; actionDescription: string } };
            expect(prompt.data.toolId).toBe(GATED_TOOL);
            expect(prompt.data.actionDescription.length).toBeGreaterThan(0);

            const toolCallChunk = await recv(client);
            expect(toolCallChunk.type).toBe('stream_chunk');
            const callState = (toolCallChunk.data as { state: { rawResponse: { toolCall: { name: string } } } }).state;
            expect(callState.rawResponse.toolCall.name).toBe(GATED_TOOL);

            // Confirm: approve. The reader was free to receive THIS frame while the turn
            // was parked — proving the turn runs as a background task.
            client.sendAction({ action: 'confirm_tool_action', requestId: 'r-confirm', sessionId, approved: true });

            // Collect the resumed stream: confirm ack, tool result chunk, tokens, terminal.
            const tokens: string[] = [];
            const toolResults: ToolResult[] = [];
            let sawAck = false;
            for (;;) {
                const event = await recv(client);
                if (event.type === 'immediate_response' && event.status === 200) {
                    sawAck = true;
                    expect((event.data as { approved: boolean }).approved).toBe(true);
                } else if (event.type === 'stream_chunk') {
                    const tr = toolResultOf(event);
                    if (tr) toolResults.push(tr);
                } else if (event.type === 'stream_token') {
                    tokens.push(event.token as string);
                } else if (event.type === 'eventual_response') {
                    expect(event.status).toBe(200);
                    const inner = (event.data as { data: { response: { responseParts: string[] } } }).data;
                    expect(inner.response.responseParts).toEqual(['Done — record 42 was deleted.']);
                    break;
                }
            }

            expect(sawAck).toBe(true);
            expect(tokens.join('')).toBe('Done — record 42 was deleted.');
            // The approved tool actually ran — its real result reached the model.
            expect(toolResults.some((tr) => tr.name === GATED_TOOL && tr.result.includes('deleted'))).toBe(true);
            expect(toolResults.every((tr) => !tr.result.includes('Denied by human'))).toBe(true);
        } finally {
            await client.close();
        }
    });

    it('reject → the tool is blocked but the turn still completes', async () => {
        server = await start([GATED_TOOL]);
        const client = await TestClient.connect(server.url);
        try {
            const sessionId = await createSession(client);
            client.sendAction({ action: 'send_message', requestId: 'r-msg', sessionId, message: 'delete it' });

            const ack = await recv(client);
            expect(ack.type).toBe('immediate_response');
            expect(ack.status).toBe(202);

            const confirm = await recv(client);
            expect(confirm.type).toBe('write_confirmation_required');
            // Consume the deferred toolCall chunk.
            const toolCallChunk = await recv(client);
            expect(toolCallChunk.type).toBe('stream_chunk');

            // Reject → the engine feeds the model a "Denied by human" result; the tool
            // never runs, but the turn still completes.
            client.sendAction({ action: 'confirm_tool_action', requestId: 'r-confirm', sessionId, approved: false });

            const toolResults: ToolResult[] = [];
            let sawRejectAck = false;
            for (;;) {
                const event = await recv(client);
                if (event.type === 'immediate_response' && event.status === 200) {
                    sawRejectAck = true;
                    expect((event.data as { approved: boolean }).approved).toBe(false);
                } else if (event.type === 'stream_chunk') {
                    const tr = toolResultOf(event);
                    if (tr) toolResults.push(tr);
                } else if (event.type === 'eventual_response') {
                    break;
                }
            }

            expect(sawRejectAck).toBe(true);
            // The rejected tool was blocked — the model saw a denial, not the result.
            expect(toolResults.some((tr) => tr.result.includes('Denied by human'))).toBe(true);
            expect(toolResults.every((tr) => !tr.result.includes('Record 42 deleted'))).toBe(true);
        } finally {
            await client.close();
        }
    });

    it('confirm with no parked turn → NO_PENDING_CONFIRMATION (never silently approves)', async () => {
        server = await start([GATED_TOOL]);
        const client = await TestClient.connect(server.url);
        try {
            const sessionId = await createSession(client);
            client.sendAction({ action: 'confirm_tool_action', requestId: 'r-confirm', sessionId, approved: true });
            const err = await recv(client);
            expect(err.type).toBe('error');
            expect((err.error as { code: string }).code).toBe('NO_PENDING_CONFIRMATION');
        } finally {
            await client.close();
        }
    });

    it('confirm with non-boolean approved → VALIDATION_ERROR (fails closed)', async () => {
        server = await start([GATED_TOOL]);
        const client = await TestClient.connect(server.url);
        try {
            const sessionId = await createSession(client);
            client.sendAction({ action: 'confirm_tool_action', requestId: 'r-confirm', sessionId, approved: 'yes' });
            const err = await recv(client);
            expect(err.type).toBe('error');
            expect((err.error as { code: string }).code).toBe('VALIDATION_ERROR');
        } finally {
            await client.close();
        }
    });
});
