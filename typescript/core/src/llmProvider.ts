/**
 * An `LlmProvider` seam over the LLM call so the agentic loop can be unit-tested
 * deterministically, without a live model or network.
 *
 * The agent already takes an injected OpenAI-compatible chat client
 * ({@link ChatClientLike}). This module *formalizes* that as the provider seam —
 * `LlmProvider` is an alias of `ChatClientLike`, so the existing `SmoothAgent`
 * constructor is unchanged and backward compatible (the real `openai` SDK still
 * satisfies it).
 *
 * It also ships a reusable, exported {@link MockLlmProvider} that replaces the
 * ad-hoc fake clients the tests rolled by hand. The mock:
 *
 * - is constructed with a script of responses — plain text, tool-call responses,
 *   and errors;
 * - returns them in FIFO order across calls;
 * - records each request (the messages + tool specs it was given) so a test can
 *   assert on what the agent sent.
 *
 * This mirrors the BEHAVIOR of the Rust reference's `MockLlmClient`
 * (`rust/smooth-operator-core/src/llm_provider.rs`). The Rust reference also
 * exposes streaming (`chat_stream`) and structured-output (`chat_structured`)
 * methods; this engine's agent loop only uses the single non-streaming chat call,
 * so the provider seam covers that one surface. Streaming / structured-output
 * land when those features land in this engine.
 */

import type { ChatClientLike } from './agent.js';

/** The LLM call surface the agent loop depends on. Identical to {@link ChatClientLike}. */
export type LlmProvider = ChatClientLike;

/** An OpenAI-shaped assistant message — the `choices[0].message` the agent reads. */
export interface ScriptedMessage {
    content: string | null;
    tool_calls?: Array<{ id: string; function: { name: string; arguments: string } }> | null;
}

/** Build a plain-text scripted response (no tool calls). */
export function textResponse(content: string): ScriptedMessage {
    return { content };
}

/** Build a scripted response that requests a single tool call. */
export function toolCallResponse(id: string, name: string, args: string): ScriptedMessage {
    return { content: null, tool_calls: [{ id, function: { name, arguments: args } }] };
}

/** One request the mock received, captured for assertions. */
export interface RecordedCall {
    /** The full request body passed to `chat.completions.create`. */
    body: Record<string, unknown>;
    /** The messages passed on this call. */
    messages: Array<Record<string, unknown>>;
    /** The tool specs offered to the model, if any. */
    tools?: Array<Record<string, unknown>>;
}

/** A scripted outcome: either a response message or an error to throw. */
type Outcome = { kind: 'message'; message: ScriptedMessage } | { kind: 'error'; message: string };

/**
 * A deterministic {@link LlmProvider} for tests. Script the responses it should
 * return (FIFO), drive your code, then assert on {@link MockLlmProvider.calls}.
 *
 * Construct empty and build up fluently (`pushText` / `pushToolCall` / `pushError`),
 * or pass an initial script of {@link ScriptedMessage}s.
 *
 * @example
 * const mock = new MockLlmProvider();
 * mock.pushText('hello there');
 * const agent = new SmoothAgent(mock, {});
 * const result = await agent.run('hi');
 * expect(result.text).toBe('hello there');
 * expect(mock.callCount).toBe(1);
 */
export class MockLlmProvider implements ChatClientLike {
    private readonly script: Outcome[] = [];
    private readonly recorded: RecordedCall[] = [];

    constructor(script: ScriptedMessage[] = []) {
        this.script = script.map((message) => ({ kind: 'message', message }));
    }

    // ── scripting (fluent: each returns this) ────────────────────────────────

    /** Queue a raw OpenAI-shaped assistant message for the next call. */
    pushResponse(message: ScriptedMessage): this {
        this.script.push({ kind: 'message', message });
        return this;
    }

    /** Queue a plain-text response for the next call. */
    pushText(content: string): this {
        return this.pushResponse(textResponse(content));
    }

    /** Queue a single-tool-call response for the next call. */
    pushToolCall(id: string, name: string, args: string): this {
        return this.pushResponse(toolCallResponse(id, name, args));
    }

    /** Queue an error to be thrown on the next call. */
    pushError(message: string): this {
        this.script.push({ kind: 'error', message });
        return this;
    }

    // ── recordings ───────────────────────────────────────────────────────────

    /** Every request the mock has received so far, in order. */
    get calls(): readonly RecordedCall[] {
        return this.recorded;
    }

    /** Number of requests received. */
    get callCount(): number {
        return this.recorded.length;
    }

    /** The most recent request, if any. */
    get lastCall(): RecordedCall | undefined {
        return this.recorded[this.recorded.length - 1];
    }

    // ── the ChatClientLike surface ───────────────────────────────────────────

    readonly chat = {
        completions: {
            create: async (body: Record<string, unknown>) => {
                this.recorded.push({
                    body,
                    messages: (body.messages as Array<Record<string, unknown>>) ?? [],
                    tools: body.tools as Array<Record<string, unknown>> | undefined,
                });
                const outcome = this.script.shift();
                if (outcome?.kind === 'error') throw new Error(outcome.message);
                // Empty script: a benign terminal text response so loops don't hang.
                const message: ScriptedMessage = outcome?.message ?? { content: '' };
                return { choices: [{ message }] };
            },
        },
    };
}
