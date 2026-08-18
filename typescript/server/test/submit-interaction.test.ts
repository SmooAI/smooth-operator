/**
 * Rich Interactions (`choices` kind) end-to-end over the WS server — the raise →
 * `interaction_required` → `submit_interaction` → resume path, plus the retryable
 * `interaction_invalid` and the text-only conversational fallback.
 *
 * Boots the real TS WS server (the `choices` kind is hosted by default) with a
 * scripted {@link MockLlmProvider} that calls `request_choices` on turn one, then
 * drives the full seam over a real `ws` client — the TS parity of the Rust server's
 * interaction integration coverage. The `submit_interaction` frame arrives on the
 * same connection's reader while the turn is parked, proving the turn runs as a
 * background task. Cross-checked against the shared `choices` fixtures.
 */
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { MockLlmProvider } from '@smooai/smooth-operator-core';
import { afterEach, describe, expect, it } from 'vitest';

import { ChoicesKind } from '../src/choices.js';
import { serve, type RunningServer } from '../src/server.js';
import { TestClient } from './wsClient.js';

const __dirname = dirname(fileURLToPath(import.meta.url));
const SPEC_DIR = join(__dirname, '..', '..', '..', 'spec');
const fixtures = JSON.parse(readFileSync(join(SPEC_DIR, 'conformance', 'fixtures.json'), 'utf8')) as Record<string, { instance: Record<string, unknown> } | string>;
function fixture(name: string): Record<string, unknown> {
    const entry = fixtures[name];
    if (!entry || typeof entry === 'string') throw new Error(`missing fixture ${name}`);
    return entry.instance;
}

const CHOICES_SPEC = fixture('choices_spec');
const CHOICES_VALUES = fixture('choices_values');
const CHOICES_PAYLOAD = fixture('choices_payload');

/** Turn 1 raises the `choices` interaction; turn 2 wraps up after it resolves. */
function scriptedMock(): MockLlmProvider {
    const mock = new MockLlmProvider();
    mock.pushToolCall('call-1', 'request_choices', JSON.stringify({ questions: (CHOICES_SPEC as { questions: unknown }).questions, reason: 'to route you to the right team' }));
    mock.pushText('Thanks — routing you now.');
    return mock;
}

async function start(): Promise<RunningServer> {
    return serve({ chatClient: scriptedMock() });
}

/** Drive create_conversation_session with the given render capabilities and return the session id. */
async function createSession(client: TestClient, supports?: string[]): Promise<string> {
    client.sendAction({
        action: 'create_conversation_session',
        requestId: 'r-create',
        agentId: '11111111-1111-1111-1111-111111111111',
        userName: 'Alice',
        userEmail: 'alice@example.com',
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

describe('Rich Interactions — choices raise / submit_interaction / resume', () => {
    let server: RunningServer | undefined;
    afterEach(async () => {
        await server?.close();
        server = undefined;
    });

    it('rich channel: raise parks, submit resumes with the canonical payload, turn completes', async () => {
        server = await start();
        const client = await TestClient.connect(server.url);
        try {
            const sessionId = await createSession(client, ['choice_chips']);
            client.sendAction({ action: 'send_message', requestId: 'r-msg', sessionId, message: 'I need help choosing' });

            // The turn parks on the raise: wait for interaction_required.
            const { terminal: required } = await client.receiveUntil('interaction_required');
            expect(required.requestId).toBe('r-msg');
            const prompt = innerData(required);
            expect(prompt.kind).toBe('choices');
            // The carried spec is the CANONICAL form (every question stamped with
            // multiSelect + each option with a description), so it is richer than the
            // sample fixture — assert the load-bearing shape rather than byte-equality.
            const questions = (prompt.spec as { questions: { header: string; options: { label: string }[]; multiSelect: boolean }[] }).questions;
            expect(questions.map((q) => q.header)).toEqual(['Plan', 'Topics']);
            expect(questions[0]!.options.map((o) => o.label)).toEqual(['Basic', 'Pro']);
            expect(questions[1]!.multiSelect).toBe(true);
            // The canonical spec still validates the shared values into the shared payload.
            expect(new ChoicesKind().validate(prompt.spec, CHOICES_VALUES).ok).toBe(true);
            expect(typeof prompt.reason).toBe('string');
            const interactionId = prompt.interactionId as string;
            expect(interactionId.length).toBeGreaterThan(0);

            // Submit the visitor's picks (the shared values fixture) — arrives on the reader
            // while the turn is parked, proving the turn runs as a background task.
            client.sendAction({ action: 'submit_interaction', requestId: 'r-msg', sessionId, interactionId, kind: 'choices', values: CHOICES_VALUES });

            let sawAck = false;
            let resumedToolResult: string | undefined;
            for (;;) {
                const event = await client.receive();
                if (event.type === 'immediate_response' && event.status === 200) {
                    sawAck = true;
                    const data = event.data as { kind?: string; values?: unknown };
                    expect(data.kind).toBe('choices');
                    // The ack carries the canonical, validated payload values.
                    expect(data.values).toEqual((CHOICES_PAYLOAD as { values: unknown }).values);
                } else if (event.type === 'stream_chunk') {
                    const tr = (event.data as { state?: { rawResponse?: { toolResult?: { name: string; result: string } } } }).state?.rawResponse?.toolResult;
                    if (tr?.name === 'request_choices') resumedToolResult = tr.result;
                } else if (event.type === 'eventual_response') {
                    const inner = (event.data as { data: { response: { responseParts: string[] } } }).data;
                    expect(inner.response.responseParts).toEqual(['Thanks — routing you now.']);
                    break;
                }
            }
            expect(sawAck).toBe(true);
            // The parked raise tool resumed with the submitted values — the model saw them.
            expect(resumedToolResult).toBeDefined();
            expect(resumedToolResult).toContain('submitted');
            expect(resumedToolResult).toContain('Partnerships');
        } finally {
            await client.close();
        }
    });

    it('invalid submit → interaction_invalid (retryable, stays parked); a corrected submit then resumes', async () => {
        server = await start();
        const client = await TestClient.connect(server.url);
        try {
            const sessionId = await createSession(client, ['choice_chips']);
            client.sendAction({ action: 'send_message', requestId: 'r-msg', sessionId, message: 'help' });
            const { terminal: required } = await client.receiveUntil('interaction_required');
            const interactionId = innerData(required).interactionId as string;

            // A pick that isn't offered (and a missing question) → interaction_invalid, not a
            // terminal error, and the turn stays parked.
            client.sendAction({ action: 'submit_interaction', requestId: 'r-msg', sessionId, interactionId, kind: 'choices', values: { answers: [{ header: 'Plan', options: ['Platinum'] }] } });
            const { terminal: invalid } = await client.receiveUntil('interaction_invalid');
            const detail = innerData(invalid);
            expect(detail.interactionId).toBe(interactionId);
            expect(detail.kind).toBe('choices');
            expect((detail.errors as { field: string }[]).some((e) => e.field === 'Plan')).toBe(true);

            // Resubmit correctly → the still-parked turn resumes and completes.
            client.sendAction({ action: 'submit_interaction', requestId: 'r-msg', sessionId, interactionId, kind: 'choices', values: CHOICES_VALUES });
            const { terminal: done, seen } = await client.receiveUntil('eventual_response');
            expect(done.status).toBe(200);
            expect(seen.some((e) => e.type === 'immediate_response' && e.status === 200)).toBe(true);
        } finally {
            await client.close();
        }
    });

    it('submit with a mismatched interactionId → INTERACTION_MISMATCH (never resolves a stale park)', async () => {
        server = await start();
        const client = await TestClient.connect(server.url);
        try {
            const sessionId = await createSession(client, ['choice_chips']);
            client.sendAction({ action: 'send_message', requestId: 'r-msg', sessionId, message: 'help' });
            const { terminal: required } = await client.receiveUntil('interaction_required');
            client.sendAction({ action: 'submit_interaction', requestId: 'r-msg', sessionId, interactionId: 'not-the-right-id', kind: 'choices', values: CHOICES_VALUES });
            const { terminal: err } = await client.receiveUntil('error');
            expect((err.error as { code: string }).code).toBe('INTERACTION_MISMATCH');
            // The turn is still parked (a stale id never resolved it): a correct submit still
            // resolves it and the turn completes — no hang.
            const interactionId = innerData(required).interactionId as string;
            client.sendAction({ action: 'submit_interaction', requestId: 'r-msg', sessionId, interactionId, kind: 'choices', values: CHOICES_VALUES });
            const { terminal: done } = await client.receiveUntil('eventual_response');
            expect(done.status).toBe(200);
        } finally {
            await client.close();
        }
    });

    it('submit with no parked turn → NO_PENDING_INTERACTION', async () => {
        server = await start();
        const client = await TestClient.connect(server.url);
        try {
            const sessionId = await createSession(client, ['choice_chips']);
            client.sendAction({ action: 'submit_interaction', requestId: 'r-x', sessionId, interactionId: 'whatever', values: CHOICES_VALUES });
            const { terminal: err } = await client.receiveUntil('error');
            expect((err.error as { code: string }).code).toBe('NO_PENDING_INTERACTION');
        } finally {
            await client.close();
        }
    });

    it('text-only channel (no supports): the raise degrades to the conversational fallback — no interaction_required', async () => {
        server = await start();
        const client = await TestClient.connect(server.url);
        try {
            const sessionId = await createSession(client); // no `supports` → text-only
            client.sendAction({ action: 'send_message', requestId: 'r-msg', sessionId, message: 'help me pick' });
            const { terminal: done, seen } = await client.receiveUntil('eventual_response');
            expect(done.status).toBe(200);
            // The rich card was never emitted — the turn ran to completion conversationally.
            expect(seen.some((e) => e.type === 'interaction_required')).toBe(false);
            // The raise tool returned the fallback directive, which the model saw.
            const directive = seen
                .map((e) => (e.data as { state?: { rawResponse?: { toolResult?: { name: string; result: string } } } }).state?.rawResponse?.toolResult)
                .find((tr) => tr?.name === 'request_choices')?.result;
            expect(directive).toBeDefined();
            expect(directive).toContain('conversational');
            expect(directive).toContain('submit_interaction');
        } finally {
            await client.close();
        }
    });
});
