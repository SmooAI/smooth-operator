/**
 * Scenario parity runner — the TypeScript port of the Python reference runner
 * (`python/server/tests/test_scenario_parity.py`).
 *
 * Runs every scenario in `spec/conformance/scenarios/*.json` through the TS server
 * and asserts the normalized protocol output matches. This is the shared corpus that
 * holds the five native servers (Rust · C# · Python · TypeScript · Go) to parity:
 * each language drives the *same* JSON scenarios through its own server and asserts
 * the *same* outbound event stream. When all five run this corpus green, the servers
 * are at protocol parity.
 *
 * The turn is deterministic because the engine runs on the same `MockLlmProvider`
 * script the scenario declares — no gateway, no flakiness.
 *
 * Faithful port of the Python state machine: per step, substitute `{{vars}}`, send
 * the frame, then match the ordered `expect` matchers — `status`/`statusGte`,
 * dot-path `assert`, `capture` vars, `repeat` runs, `accumulate`+`assertAccumulated`,
 * skipping non-semantic `keepalive`/`pong` frames.
 */
import { readdirSync, readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join, resolve } from 'node:path';

import { InMemoryKnowledge, MockLlmProvider } from '@smooai/smooth-operator-core';
import type { Knowledge, Tool } from '@smooai/smooth-operator-core';
import { afterEach, describe, expect, it } from 'vitest';

import type { AccessKnowledge } from '../src/frameDispatcher.js';
import { serve, type RunningServer } from '../src/server.js';
import { TestClient } from './wsClient.js';

const here = dirname(fileURLToPath(import.meta.url));
// test/ → server/ → typescript/ → repo root → spec/conformance/scenarios
const SCENARIOS_DIR = resolve(here, '..', '..', '..', 'spec', 'conformance', 'scenarios');
const SCENARIOS = readdirSync(SCENARIOS_DIR)
    .filter((f) => f.endsWith('.json'))
    .sort()
    .map((f) => join(SCENARIOS_DIR, f));

interface MockScriptEntry {
    kind: 'text' | 'toolCall';
    text?: string;
    id?: string;
    name?: string;
    arguments?: string;
}

interface Matcher {
    type: string;
    status?: number;
    statusGte?: number;
    assert?: Record<string, unknown>;
    capture?: Record<string, string>;
    repeat?: boolean;
    accumulate?: string;
    assertAccumulated?: string;
}

interface Step {
    send: Record<string, unknown>;
    expect: Matcher[];
}

interface ToolSpec {
    name: string;
    description?: string;
    parameters?: Record<string, unknown>;
    result: string;
}

/** A knowledge doc to seed into the server's in-memory KB before a grounded turn. */
interface KnowledgeDoc {
    source: string;
    content: string;
}

interface Scenario {
    name: string;
    description?: string;
    mockLlmScript?: MockScriptEntry[];
    server?: { tools?: ToolSpec[]; confirmTools?: string[]; knowledge?: KnowledgeDoc[] };
    steps: Step[];
}

/**
 * Resolve a dotted path (`data.data.response.responseParts`) into a nested object.
 * A numeric path segment indexes a list/array (`data.data.citations.0.id`) — JS
 * string-keys arrays, so the same lookup works for objects and arrays, and a `null`
 * intermediate short-circuits to `undefined` instead of throwing.
 */
function dot(obj: Record<string, unknown>, path: string): unknown {
    let cur: unknown = obj;
    for (const part of path.split('.')) {
        if (cur === null || cur === undefined) return undefined;
        cur = (cur as Record<string, unknown>)[part];
    }
    return cur;
}

/** Build the engine's deterministic mock from a scenario's `mockLlmScript`. */
function buildMock(script: MockScriptEntry[]): MockLlmProvider {
    const mock = new MockLlmProvider();
    for (const entry of script) {
        if (entry.kind === 'text') {
            mock.pushText(entry.text ?? '');
        } else if (entry.kind === 'toolCall') {
            mock.pushToolCall(entry.id ?? 'call-1', entry.name!, entry.arguments ?? '{}');
        } else {
            throw new Error(`unknown mockLlmScript kind: ${(entry as MockScriptEntry).kind}`);
        }
    }
    return mock;
}

/**
 * Build deterministic test tools from a scenario's `server.tools` directive — the
 * TS analog of the Python runner's `_build_tools`. Each tool ignores its arguments
 * and returns the spec's fixed `result` string, so a tool-call turn is fully
 * deterministic across every server.
 */
function buildTools(specs: ToolSpec[]): Tool[] {
    return specs.map((spec) => ({
        name: spec.name,
        description: spec.description ?? '',
        parameters: spec.parameters ?? { type: 'object', properties: {} },
        execute: async (_args: Record<string, unknown>): Promise<string> => spec.result,
    }));
}

/**
 * Seed an in-memory knowledge base from a scenario's `server.knowledge` directive —
 * the TS analog of the Rust/Python runner's KB seeding for the citations dimension.
 * Each doc is ingested into the engine's {@link InMemoryKnowledge}; the resulting
 * retriever is exposed (unscoped — every access sees the same KB) via the server's
 * {@link AccessKnowledge} seam, so a grounded turn carries `data.data.citations`.
 * Returns `undefined` when no docs are declared (behavior unchanged for every
 * existing scenario).
 */
function buildKnowledge(docs: KnowledgeDoc[]): AccessKnowledge | undefined {
    if (docs.length === 0) return undefined;
    const kb = new InMemoryKnowledge();
    for (const doc of docs) {
        kb.ingest(doc.content, doc.source);
    }
    return { forAccess: (): Knowledge => kb };
}

/** Replace `{{name}}` placeholders in string fields from captured vars. */
function subst(value: unknown, vars: Record<string, unknown>): unknown {
    if (typeof value === 'string' && value.startsWith('{{') && value.endsWith('}}')) {
        return vars[value.slice(2, -2)];
    }
    if (value !== null && typeof value === 'object' && !Array.isArray(value)) {
        return Object.fromEntries(Object.entries(value as Record<string, unknown>).map(([k, v]) => [k, subst(v, vars)]));
    }
    return value;
}

/** Next protocol event, skipping non-semantic keepalive/pong frames. */
async function nextEvent(client: TestClient): Promise<Record<string, unknown>> {
    for (;;) {
        const event = await client.receive();
        if (event.type !== 'keepalive' && event.type !== 'pong') return event;
    }
}

/** Match the outbound event stream against an ordered list of matchers. */
async function matchExpected(client: TestClient, matchers: Matcher[], vars: Record<string, unknown>): Promise<void> {
    let pending: Record<string, unknown> | undefined; // one-event lookahead when a `repeat` matcher overruns
    for (const m of matchers) {
        let accumulated = '';
        for (;;) {
            const event = pending ?? (await nextEvent(client));
            pending = undefined;
            if (m.repeat && event.type !== m.type) {
                // the repeated run ended; this event belongs to the next matcher
                pending = event;
                break;
            }
            expect(event.type, `expected ${m.type}, got ${String(event.type)}`).toBe(m.type);
            if (m.status !== undefined) {
                expect(event.status, `${m.type}: status`).toBe(m.status);
            }
            if (m.statusGte !== undefined) {
                expect(event.status as number, `${m.type}: status >= ${m.statusGte}`).toBeGreaterThanOrEqual(m.statusGte);
            }
            for (const [path, expected] of Object.entries(m.assert ?? {})) {
                expect(dot(event, path), `${m.type}: ${path}`).toEqual(expected);
            }
            for (const [varName, path] of Object.entries(m.capture ?? {})) {
                vars[varName] = dot(event, path);
            }
            if (m.accumulate !== undefined) {
                accumulated += String(event[m.accumulate]);
            }
            if (!m.repeat) break;
        }
        if (m.assertAccumulated !== undefined) {
            expect(accumulated, `accumulated for ${m.type}`).toBe(m.assertAccumulated);
        }
    }
}

describe('scenario parity — TS server runs the shared conformance corpus', () => {
    let server: RunningServer | undefined;

    afterEach(async () => {
        await server?.close();
        server = undefined;
    });

    for (const path of SCENARIOS) {
        const scenario = JSON.parse(readFileSync(path, 'utf8')) as Scenario;
        it(scenario.name, async () => {
            server = await serve({
                chatClient: buildMock(scenario.mockLlmScript ?? []),
                // Seed the in-memory KB when the scenario declares `server.knowledge`,
                // so a grounded turn carries `data.data.citations`. Absent → no KB.
                knowledge: buildKnowledge(scenario.server?.knowledge ?? []),
                tools: buildTools(scenario.server?.tools ?? []),
                // A tool listed in `server.confirmTools` is gated behind write-confirmation
                // HITL: the turn parks and emits `write_confirmation_required` until the
                // client sends `confirm_tool_action`. Empty/absent → no gating (every
                // existing scenario). Mirrors the Python runner's `confirmTools` directive.
                confirmTools: scenario.server?.confirmTools ?? [],
            });
            const client = await TestClient.connect(server.url);
            const vars: Record<string, unknown> = {};
            try {
                for (const step of scenario.steps) {
                    client.sendAction(subst(step.send, vars) as Record<string, unknown>);
                    await matchExpected(client, step.expect, vars);
                }
            } finally {
                await client.close();
            }
        });
    }
});
