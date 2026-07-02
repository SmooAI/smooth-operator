/**
 * Integration: per-agent config + workflow over a REAL WebSocket server (SMOODEV-590).
 *
 * Proves end-to-end that (a) an agent's own `instructions` drive its system prompt,
 * (b) two agents on the same server behave differently (per-agent isolation), (c) a
 * `conversationWorkflow` renders the current step and the post-turn judge advances it
 * across turns, and (d) a malformed config degrades to the default without crashing.
 */
import { MockLlmProvider } from '@smooai/smooth-operator-core';
import { afterEach, describe, expect, it } from 'vitest';

import { parseAgentConfig, StaticAgentConfigResolver } from '../src/agentConfig.js';
import { serve, type RunningServer } from '../src/server.js';
import { InMemorySessionStore } from '../src/sessionStore.js';
import { TestClient } from './wsClient.js';

const AGENT_A = '11111111-1111-1111-1111-111111111111';
const AGENT_B = '22222222-2222-2222-2222-222222222222';

/** The system prompt the engine sent on the nth (0-based) model call to `mock`. */
function systemPromptOf(mock: MockLlmProvider, callIndex: number): string {
    const call = mock.calls[callIndex]!;
    const sys = call.messages.find((m) => (m as { role?: string }).role === 'system') as { content?: string } | undefined;
    return sys?.content ?? '';
}

async function openSession(client: TestClient, agentId: string): Promise<string> {
    client.sendAction({ action: 'create_conversation_session', requestId: `cs-${agentId}`, agentId });
    const created = await client.receive();
    return (created.data as Record<string, unknown>).sessionId as string;
}

async function sendAndDrain(client: TestClient, sessionId: string, message: string): Promise<void> {
    client.sendAction({ action: 'send_message', requestId: `sm-${Math.random()}`, sessionId, message });
    await client.receiveUntil('eventual_response');
}

describe('per-agent config over a real WebSocket', () => {
    let server: RunningServer | undefined;

    afterEach(async () => {
        await server?.close();
        server = undefined;
    });

    it("honors an agent's own instructions in the system prompt", async () => {
        const mock = new MockLlmProvider().pushText('Ada here — happy to help with billing.');
        const agentConfig = new StaticAgentConfigResolver({ [AGENT_A]: { instructions: 'You are Ada, a billing specialist. Only discuss billing.' } });
        server = await serve({ chatClient: mock, agentConfig });
        const client = await TestClient.connect(server.url);

        const sessionId = await openSession(client, AGENT_A);
        await sendAndDrain(client, sessionId, 'Why was I charged twice?');

        expect(systemPromptOf(mock, 0)).toContain('You are Ada, a billing specialist.');
        await client.close();
    });

    it('keeps two agents on the same server isolated', async () => {
        const mock = new MockLlmProvider().pushText('reply A').pushText('reply B');
        const agentConfig = new StaticAgentConfigResolver({
            [AGENT_A]: { instructions: 'You are Ada, billing.' },
            [AGENT_B]: { instructions: 'You are Boris, scheduling.' },
        });
        server = await serve({ chatClient: mock, agentConfig });
        const client = await TestClient.connect(server.url);

        const sessionA = await openSession(client, AGENT_A);
        await sendAndDrain(client, sessionA, 'q1');
        const sessionB = await openSession(client, AGENT_B);
        await sendAndDrain(client, sessionB, 'q2');

        expect(systemPromptOf(mock, 0)).toContain('You are Ada, billing.');
        expect(systemPromptOf(mock, 0)).not.toContain('Boris');
        expect(systemPromptOf(mock, 1)).toContain('You are Boris, scheduling.');
        expect(systemPromptOf(mock, 1)).not.toContain('Ada');
        await client.close();
    });

    it('falls back to the default prompt for an un-configured agent', async () => {
        const mock = new MockLlmProvider().pushText('generic reply');
        server = await serve({ chatClient: mock, agentConfig: new StaticAgentConfigResolver({}) });
        const client = await TestClient.connect(server.url);

        const sessionId = await openSession(client, AGENT_A);
        await sendAndDrain(client, sessionId, 'hello');

        expect(systemPromptOf(mock, 0)).toContain('helpful customer support agent');
        await client.close();
    });

    it('applies the greeting on the first turn only, isolated per agent', async () => {
        const mock = new MockLlmProvider().pushText('r1').pushText('r2').pushText('rB');
        const agentConfig = new StaticAgentConfigResolver({
            [AGENT_A]: { instructions: 'You are Ada.', greeting: 'Thanks for calling Acme!' },
            [AGENT_B]: { instructions: 'You are Boris.', greeting: 'Welcome to Beta Co!' },
        });
        server = await serve({ chatClient: mock, agentConfig });
        const client = await TestClient.connect(server.url);

        const sessionA = await openSession(client, AGENT_A);
        await sendAndDrain(client, sessionA, 'turn 1');
        await sendAndDrain(client, sessionA, 'turn 2');

        // Turn 1 (call 0) carries A's greeting; turn 2 (call 1) does not.
        expect(systemPromptOf(mock, 0)).toContain('Thanks for calling Acme!');
        expect(systemPromptOf(mock, 1)).not.toContain('Thanks for calling Acme!');
        expect(systemPromptOf(mock, 1)).not.toContain('GreetingAwareness');

        // Agent B's first turn (call 2) carries B's greeting, not A's.
        const sessionB = await openSession(client, AGENT_B);
        await sendAndDrain(client, sessionB, 'turn 1');
        expect(systemPromptOf(mock, 2)).toContain('Welcome to Beta Co!');
        expect(systemPromptOf(mock, 2)).not.toContain('Acme');

        await client.close();
    });

    it('renders the workflow step and advances it across turns as the judge says yes', async () => {
        // Turn 1: reply (script[0]) + judge "yes" (script[1]) → advance greet → qualify.
        // Turn 2: reply (script[2]) + judge "no"  (script[3]) → stay on qualify.
        const mock = new MockLlmProvider().pushText('Hi Sam!').pushText('{"verdict":"yes"}').pushText('What size is your team?').pushText('{"verdict":"no"}');
        const store = new InMemorySessionStore();
        const agentConfig = new StaticAgentConfigResolver({
            [AGENT_A]: {
                conversationWorkflow: {
                    goal: 'Qualify the lead',
                    steps: [
                        { id: 'greet', intent: 'Greet by name', criteria: 'Visitor greeted' },
                        { id: 'qualify', intent: 'Ask team size', criteria: 'Team size captured' },
                    ],
                },
            },
        });
        server = await serve({ chatClient: mock, store, agentConfig });
        const client = await TestClient.connect(server.url);

        const sessionId = await openSession(client, AGENT_A);

        await sendAndDrain(client, sessionId, 'hi');
        // Turn 1's system prompt rendered the FIRST step.
        expect(systemPromptOf(mock, 0)).toContain('CURRENT STEP (1/2): greet');
        // Judge said yes → the pointer advanced and is persisted.
        expect((await store.getSession(sessionId))?.currentStepId).toBe('qualify');

        await sendAndDrain(client, sessionId, 'we are a team');
        // Turn 2's system prompt (call index 2 — index 1 was turn 1's judge) rendered
        // the SECOND step, proving the advance carried across turns.
        expect(systemPromptOf(mock, 2)).toContain('CURRENT STEP (2/2): qualify');
        // Judge said no → stays on qualify.
        expect((await store.getSession(sessionId))?.currentStepId).toBe('qualify');

        await client.close();
    });

    it('degrades to the default flow when the agent config carries a malformed workflow', async () => {
        const mock = new MockLlmProvider().pushText('generic reply');
        // A record with a broken workflow (empty steps) and no instructions → parseAgentConfig
        // yields undefined, so the turn uses the default prompt and never crashes.
        const config = parseAgentConfig({ conversation_workflow: { goal: 'g', steps: [] } });
        const agentConfig = new StaticAgentConfigResolver(config ? { [AGENT_A]: config } : {});
        server = await serve({ chatClient: mock, agentConfig });
        const client = await TestClient.connect(server.url);

        const sessionId = await openSession(client, AGENT_A);
        await sendAndDrain(client, sessionId, 'hello');

        expect(systemPromptOf(mock, 0)).toContain('helpful customer support agent');
        expect(systemPromptOf(mock, 0)).not.toContain('CURRENT STEP');
        await client.close();
    });

    it('survives a resolver that throws, falling back to the default prompt', async () => {
        const mock = new MockLlmProvider().pushText('generic reply');
        const throwing = {
            resolve() {
                throw new Error('db down');
            },
        };
        server = await serve({ chatClient: mock, agentConfig: throwing });
        const client = await TestClient.connect(server.url);

        const sessionId = await openSession(client, AGENT_A);
        await sendAndDrain(client, sessionId, 'hello');

        expect(systemPromptOf(mock, 0)).toContain('helpful customer support agent');
        await client.close();
    });
});
