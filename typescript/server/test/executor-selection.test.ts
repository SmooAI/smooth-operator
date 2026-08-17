/**
 * Tests for the durable-executor selection seam — the TS sibling of the Rust
 * server's `runner.rs` `durable_requested` / `turn_executor` tests.
 *
 * Getting this backwards would silently change how every deployed turn runs, so
 * the opt-in parse and the injection precedence are pinned here. Uses a FAKE
 * injected executor (no Temporal dependency) to prove: env on + injected ⇒ the
 * durable executor is used; nothing injected ⇒ the in-process default.
 */

import { InProcessExecutor } from '@smooai/smooth-operator-core';
import type { AgentExecutor, AgentRunResponse, SmoothAgent, StreamEvent } from '@smooai/smooth-operator-core';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { DURABLE_EXECUTOR_ENV, durableRequested, turnExecutor } from '../src/executorSelection.js';

/** A stand-in durable backend — proves an injected executor survives the trip. */
class FakeExecutor implements AgentExecutor {
    async execute(): Promise<AgentRunResponse> {
        return { text: '', iterations: 0, toolCalls: 0, usage: { promptTokens: 0, completionTokens: 0 }, costUsd: 0, budgetExceeded: false };
    }

    async *executeStreaming(): AsyncGenerator<StreamEvent> {
        yield { type: 'done', response: await this.execute() };
    }
}

// The runner passes the agent + message; the seam never touches them, so an empty
// object is enough for these selection tests.
const noopAgent = {} as unknown as SmoothAgent;

afterEach(() => {
    vi.restoreAllMocks();
});

describe('durableRequested', () => {
    it('is opt-in only — on for 1/true/on/yes (any case/space), off for everything else', () => {
        for (const on of ['1', 'true', 'TRUE', ' on ', 'yes']) {
            expect(durableRequested(on)).toBe(true);
        }
        for (const off of ['', ' ', '0', 'false', 'off', 'no', 'maybe']) {
            expect(durableRequested(off)).toBe(false);
        }
        expect(durableRequested(undefined)).toBe(false);
    });
});

describe('turnExecutor', () => {
    it('uses an injected executor verbatim — the same handle, env not consulted', () => {
        const injected = new FakeExecutor();
        expect(turnExecutor(injected, { [DURABLE_EXECUTOR_ENV]: '1' })).toBe(injected);
        expect(turnExecutor(injected, {})).toBe(injected);
    });

    it('env on + injected ⇒ the durable executor is used', async () => {
        const injected = new FakeExecutor();
        const spy = vi.spyOn(injected, 'executeStreaming');
        const selected = turnExecutor(injected, { [DURABLE_EXECUTOR_ENV]: 'true' });
        // Drain the seam to prove the injected backend is what actually runs.
        for await (const _ of selected.executeStreaming(noopAgent, 'hi', [])) {
            // consume
        }
        expect(selected).toBe(injected);
        expect(spy).toHaveBeenCalledOnce();
    });

    it('env off + nothing injected ⇒ a fresh in-process executor', () => {
        const a = turnExecutor(undefined, {});
        const b = turnExecutor(undefined, {});
        expect(a).toBeInstanceOf(InProcessExecutor);
        expect(b).toBeInstanceOf(InProcessExecutor);
        expect(a).not.toBe(b); // each fallback builds its own
    });

    it('env on + nothing injected ⇒ warns and falls back to in-process (never a fake-durable turn)', () => {
        const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
        const selected = turnExecutor(undefined, { [DURABLE_EXECUTOR_ENV]: 'yes' });
        expect(selected).toBeInstanceOf(InProcessExecutor);
        expect(warn).toHaveBeenCalledOnce();
        expect(warn.mock.calls[0][0]).toContain(DURABLE_EXECUTOR_ENV);
    });
});
