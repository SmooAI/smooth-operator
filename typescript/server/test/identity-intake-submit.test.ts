/**
 * Rich Interactions (`identity_intake` kind) end-to-end over the WS server — the
 * raise → `interaction_required` → `submit_interaction` → resume path AND the
 * kind-routed **host effect**: a valid submit stamps the captured contacts onto
 * the session (`userName` / `contactEmail` / `contactPhone`), the keys the OTP
 * contact seam reads, so the caller is immediately OTP-contactable.
 *
 * Boots the real TS WS server (the `identity_intake` kind is hosted by default)
 * with a scripted {@link MockLlmProvider} and drives the seam over a real `ws`
 * client — asserting BOTH transport paths run the host effect: the rich WS
 * `submit_interaction` action (host effect in the frame dispatcher) and the
 * text-only conversational fallback (host effect inside the `submit_interaction`
 * tool). Cross-checked against the shared `identity_intake` fixtures.
 */
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { MockLlmProvider } from '@smooai/smooth-operator-core';
import { afterEach, describe, expect, it } from 'vitest';

import { serve, type RunningServer } from '../src/server.js';
import { InMemorySessionStore } from '../src/sessionStore.js';
import { isContactEmpty } from '../src/otp.js';
import { TestClient } from './wsClient.js';

const __dirname = dirname(fileURLToPath(import.meta.url));
const SPEC_DIR = join(__dirname, '..', '..', '..', 'spec');
const fixtures = JSON.parse(readFileSync(join(SPEC_DIR, 'conformance', 'fixtures.json'), 'utf8')) as Record<string, { instance: Record<string, unknown> } | string>;
function fixture(name: string): Record<string, unknown> {
    const entry = fixtures[name];
    if (!entry || typeof entry === 'string') throw new Error(`missing fixture ${name}`);
    return entry.instance;
}

const INTAKE_SPEC = fixture('identity_intake_spec');
const INTAKE_VALUES = fixture('identity_intake_values');
const INTAKE_PAYLOAD = fixture('identity_intake_payload');

/** Turn 1 raises the `identity_intake` interaction; turn 2 wraps up after it resolves (rich path). */
function richMock(): MockLlmProvider {
    return new MockLlmProvider()
        .pushToolCall('call-1', 'request_identity_intake', JSON.stringify({ fields: (INTAKE_SPEC as { fields: unknown }).fields, reason: 'to send you the quote' }))
        .pushText('Thanks — I have your details.');
}

/**
 * Text-only path: the raise returns the conversational directive, then the model
 * submits the collected values through the generic `submit_interaction` *tool*
 * (which runs the host effect), then wraps up.
 */
function fallbackMock(): MockLlmProvider {
    return new MockLlmProvider()
        .pushToolCall('call-1', 'request_identity_intake', JSON.stringify({ fields: (INTAKE_SPEC as { fields: unknown }).fields, reason: 'to send you the quote' }))
        .pushToolCall('call-2', 'submit_interaction', JSON.stringify({ kind: 'identity_intake', values: INTAKE_VALUES }))
        .pushText('Thanks — I have your details.');
}

async function createSession(client: TestClient, supports?: string[]): Promise<string> {
    client.sendAction({
        action: 'create_conversation_session',
        requestId: 'r-create',
        agentId: '11111111-1111-1111-1111-111111111111',
        userName: 'Alice',
        // No userEmail: prove the intake host effect is the source of the stamped contacts.
        ...(supports ? { supports } : {}),
    });
    for (;;) {
        const event = await client.receive();
        if (event.type === 'immediate_response') return (event.data as { sessionId: string }).sessionId;
    }
}

function innerData(event: Record<string, unknown>): Record<string, unknown> {
    return (event.data as { data: Record<string, unknown> }).data;
}

describe('Rich Interactions — identity_intake raise / submit / resume + host effect', () => {
    let server: RunningServer | undefined;
    afterEach(async () => {
        await server?.close();
        server = undefined;
    });

    it('rich channel: submit resumes with the canonical payload AND stamps the session contacts (OTP-contactable)', async () => {
        const store = new InMemorySessionStore();
        server = await serve({ chatClient: richMock(), store });
        const client = await TestClient.connect(server.url);
        try {
            const sessionId = await createSession(client, ['identity_form']);

            // Precondition: no contact on the session before the intake resolves.
            const before = await store.getSession(sessionId);
            expect(before?.contactEmail).toBeUndefined();
            expect(before?.contactPhone).toBeUndefined();

            client.sendAction({ action: 'send_message', requestId: 'r-msg', sessionId, message: 'I want a quote' });

            const { terminal: required } = await client.receiveUntil('interaction_required');
            const prompt = innerData(required);
            expect(prompt.kind).toBe('identity_intake');
            const interactionId = prompt.interactionId as string;

            client.sendAction({ action: 'submit_interaction', requestId: 'r-msg', sessionId, interactionId, kind: 'identity_intake', values: INTAKE_VALUES });

            let sawAck = false;
            for (;;) {
                const event = await client.receive();
                if (event.type === 'immediate_response' && event.status === 200) {
                    sawAck = true;
                    const data = event.data as { kind?: string; values?: unknown };
                    expect(data.kind).toBe('identity_intake');
                    expect(data.values).toEqual((INTAKE_PAYLOAD as { values: unknown }).values);
                } else if (event.type === 'eventual_response') {
                    break;
                }
            }
            expect(sawAck).toBe(true);

            // The host effect stamped the captured contacts onto the session — the SAME
            // keys the OTP contact seam reads, so the caller is now OTP-contactable.
            const after = await store.getSession(sessionId);
            expect(after?.userName).toBe('Alice Example');
            expect(after?.contactEmail).toBe('alice@example.com');
            expect(after?.contactPhone).toBe('+15551234567');
            expect(isContactEmpty({ email: after?.contactEmail, phone: after?.contactPhone })).toBe(false);
        } finally {
            await client.close();
        }
    });

    it('text-only channel: the conversational-fallback submit_interaction tool ALSO runs the host effect', async () => {
        const store = new InMemorySessionStore();
        server = await serve({ chatClient: fallbackMock(), store });
        const client = await TestClient.connect(server.url);
        try {
            const sessionId = await createSession(client); // no supports → text-only
            client.sendAction({ action: 'send_message', requestId: 'r-msg', sessionId, message: 'I want a quote' });

            const { seen } = await client.receiveUntil('eventual_response');
            // No rich card on a text-only channel.
            expect(seen.some((e) => e.type === 'interaction_required')).toBe(false);

            // The fallback submit ran the host effect from inside the tool.
            const after = await store.getSession(sessionId);
            expect(after?.userName).toBe('Alice Example');
            expect(after?.contactEmail).toBe('alice@example.com');
            expect(after?.contactPhone).toBe('+15551234567');
        } finally {
            await client.close();
        }
    });
});
