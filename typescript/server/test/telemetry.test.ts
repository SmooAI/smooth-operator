/**
 * Telemetry coverage for the production turn path (`TurnRunner.run`) — the TS parity of
 * `rust/smooth-operator-server/tests/telemetry.rs`.
 *
 * Drives a real streaming turn (a tool call then a text answer, via `MockLlmProvider`)
 * with an in-memory span exporter registered as the global tracer provider — no live
 * OTLP collector — and asserts the turn emits:
 *
 * 1. A `gen_ai.chat` span carrying `gen_ai.system`, `gen_ai.request.model`,
 *    `gen_ai.conversation.id`, `gen_ai.agent.name`, and `smooai.org_id`.
 * 2. A child `gen_ai.tool` span carrying `gen_ai.tool.name` and the (redacted)
 *    `gen_ai.tool.call.arguments` the model passed.
 */
import { MockLlmProvider } from '@smooai/smooth-operator-core';
import { trace } from '@opentelemetry/api';
import { BasicTracerProvider, InMemorySpanExporter, SimpleSpanProcessor, type ReadableSpan } from '@opentelemetry/sdk-trace-base';
import { beforeAll, describe, expect, it } from 'vitest';

import type { Frame } from '../src/protocol.js';
import { InMemorySessionStore } from '../src/sessionStore.js';
import { redactToolArguments } from '../src/telemetry.js';
import { TurnRunner } from '../src/turnRunner.js';

const exporter = new InMemorySpanExporter();

beforeAll(() => {
    // Register a global provider so `trace.getTracer` inside the runner exports here.
    trace.setGlobalTracerProvider(new BasicTracerProvider({ spanProcessors: [new SimpleSpanProcessor(exporter)] }));
});

function attr(span: ReadableSpan, key: string): unknown {
    return span.attributes[key];
}

describe('TurnRunner GenAI OTel spans', () => {
    it('emits gen_ai.chat (with org) + gen_ai.tool (with args) spans for a tool turn', async () => {
        const store = new InMemorySessionStore();
        const session = await store.createSession('agent-otel', undefined, undefined, undefined, 'org-telemetry');
        const chat = new MockLlmProvider()
            .pushToolCall('call-kb-1', 'knowledge_search', JSON.stringify({ query: 'return policy refund window' }))
            .pushText('Items are accepted within 30 days for a full refund.');
        const runner = new TurnRunner({ chatClient: chat, store, model: 'openai/gpt-4o', orgId: 'org-telemetry' });
        const sink = (_frame: Frame): void => {};

        await runner.run(session.conversationId, 'req-otel', 'what is the return policy?', sink);

        const spans = exporter.getFinishedSpans();

        // (1) The turn span carries system, model, conversation, agent, and org.
        const chatSpan = spans.find((s) => s.name === 'gen_ai.chat');
        expect(chatSpan, `expected a gen_ai.chat span; got: ${spans.map((s) => s.name).join(', ')}`).toBeDefined();
        expect(attr(chatSpan!, 'gen_ai.system')).toBe('smooth-operator');
        expect(attr(chatSpan!, 'gen_ai.request.model')).toBe('openai/gpt-4o');
        expect(attr(chatSpan!, 'gen_ai.conversation.id')).toBe(session.conversationId);
        expect(attr(chatSpan!, 'gen_ai.agent.name')).toBe('smooth-agent-chat');
        expect(attr(chatSpan!, 'smooai.org_id')).toBe('org-telemetry');

        // (2) A child tool span with the tool name + the model's arguments.
        const toolSpan = spans.find((s) => s.name === 'gen_ai.tool');
        expect(toolSpan, 'expected a gen_ai.tool span').toBeDefined();
        expect(attr(toolSpan!, 'gen_ai.tool.name')).toBe('knowledge_search');
        expect(String(attr(toolSpan!, 'gen_ai.tool.call.arguments'))).toContain('return policy refund window');

        // The tool span is a child of the turn span (same trace).
        expect(toolSpan!.spanContext().traceId).toBe(chatSpan!.spanContext().traceId);
        expect(toolSpan!.parentSpanContext?.spanId).toBe(chatSpan!.spanContext().spanId);

        // (3) Being a child is NOT enough. The OTLP ingest builds a span's attributes
        // from the resource attrs plus THAT span's own, with no parent inheritance, so
        // the tool span repeats the identifiers itself — and without gen_ai.system it
        // fails the ingest's LLM-event gate outright and is discarded, which is what
        // happened to Rust's tool spans for their entire existence.
        expect(attr(toolSpan!, 'gen_ai.system')).toBe('smooth-operator');
        expect(attr(toolSpan!, 'gen_ai.operation.name')).toBe('tool');
        expect(attr(toolSpan!, 'gen_ai.conversation.id')).toBe(session.conversationId);
        expect(attr(toolSpan!, 'smooai.org_id')).toBe('org-telemetry');

        // Must be exactly 'chat'/'tool' — the ingest takes the attribute verbatim when
        // present and its queries filter on operation_name = 'tool'.
        expect(attr(chatSpan!, 'gen_ai.operation.name')).toBe('chat');

        // (4) Cost: exactly one of the two is ever set, and a zero is never exported as
        // a real cost (it means "unpriced", not "free").
        const cost = attr(chatSpan!, 'gen_ai.usage.cost_usd');
        if (cost === undefined) {
            expect(attr(chatSpan!, 'smooai.gen_ai.cost_unavailable')).toBe('unpriced');
        } else {
            expect(Number(cost)).toBeGreaterThan(0);
            expect(attr(chatSpan!, 'smooai.gen_ai.cost_unavailable')).toBeUndefined();
        }
    });
});

describe('redactToolArguments', () => {
    it('redacts secret-named keys but keeps the rest', () => {
        const out = redactToolArguments(JSON.stringify({ query: 'weather', api_key: 'sk-live-123', nested: { authToken: 'abc' } }));
        expect(out).toContain('weather');
        expect(out).not.toContain('sk-live-123');
        expect(out).not.toContain('abc');
        expect(out).toContain('[REDACTED]');
    });

    it('passes non-JSON through as-is', () => {
        expect(redactToolArguments('not json')).toBe('not json');
    });
});
