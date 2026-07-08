/**
 * The turn runner threads the raised starvation defaults and the per-turn model-output
 * ceiling into the engine (EPIC th-1cc9fa). Driven offline with the shared
 * `MockLlmProvider`, which records each request body so we can assert on `max_tokens`.
 */
import { MockLlmProvider } from '@smooai/smooth-operator-core';
import { describe, expect, it } from 'vitest';

import { InMemorySessionStore } from '../src/sessionStore.js';
import { DEFAULT_MAX_TOKENS, TurnRunner } from '../src/turnRunner.js';
import type { Frame } from '../src/protocol.js';

/** Run one turn against a fresh conversation and return the recorded request body. */
async function runTurn(mock: MockLlmProvider, runnerOptions: Partial<ConstructorParameters<typeof TurnRunner>[0]> = {}): Promise<Record<string, unknown>> {
    const store = new InMemorySessionStore();
    const session = await store.createSession('agent-1');
    const runner = new TurnRunner({ chatClient: mock, store, ...runnerOptions });
    const sink = (_frame: Frame): void => {};
    await runner.run(session.conversationId, 'req-1', 'hello', sink);
    return mock.calls[0].body;
}

describe('TurnRunner max_tokens clamp + defaults', () => {
    it('sends the raised DEFAULT_MAX_TOKENS when no ceiling resolver is set', async () => {
        const body = await runTurn(new MockLlmProvider().pushText('hi'));
        expect(body.max_tokens).toBe(DEFAULT_MAX_TOKENS);
        expect(DEFAULT_MAX_TOKENS).toBe(8192);
    });

    it('clamps max_tokens down to the model ceiling when it is below the budget', async () => {
        // A ceiling below DEFAULT_MAX_TOKENS (8192) must win.
        const body = await runTurn(new MockLlmProvider().pushText('hi'), {
            model: 'tiny-model',
            modelCeiling: async (model) => (model === 'tiny-model' ? 4096 : undefined),
        });
        expect(body.max_tokens).toBe(4096);
    });

    it('leaves max_tokens at the budget when the ceiling is >= the budget', async () => {
        const body = await runTurn(new MockLlmProvider().pushText('hi'), {
            model: 'big-model',
            modelCeiling: async () => 65536,
        });
        expect(body.max_tokens).toBe(DEFAULT_MAX_TOKENS);
    });

    it('leaves max_tokens unclamped when the resolver returns undefined (unknown model)', async () => {
        const body = await runTurn(new MockLlmProvider().pushText('hi'), {
            model: 'mystery',
            modelCeiling: async () => undefined,
        });
        expect(body.max_tokens).toBe(DEFAULT_MAX_TOKENS);
    });

    it('resolves the ceiling for the model the turn actually uses', async () => {
        let askedFor: string | undefined;
        const body = await runTurn(new MockLlmProvider().pushText('hi'), {
            model: 'claude-haiku-4-5',
            modelCeiling: async (model) => {
                askedFor = model;
                return 8192;
            },
        });
        expect(askedFor).toBe('claude-haiku-4-5');
        expect(body.model).toBe('claude-haiku-4-5');
    });
});
