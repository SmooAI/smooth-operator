/**
 * `createTestHost` — an in-process scripted SEP host for unit-testing an
 * extension without spawning a subprocess. It plays the host side of the
 * protocol over a `linkedPair`, so tests drive `initialize`, `tool/execute`
 * (with progress + cancellation), events, ping and shutdown directly against a
 * `defineExtension(...)` object.
 */
import { Peer } from './jsonrpc.js';
import { PROTOCOL_VERSION, method } from './protocol.js';
import type { Context, HookOutcome, InitializeParams, InitializeResult, ToolExecuteResult, ToolUpdateParams } from './protocol.js';
import type { Extension } from './extension.js';
import { linkedPair } from './transport.js';

let callSeq = 0;

export interface CallToolOptions {
    /** Receives each `tool/update` the extension streams for this call. */
    onUpdate?: (update: ToolUpdateParams) => void;
    /** Abort the call — the host sends `$/cancel` and the promise rejects. */
    signal?: AbortSignal;
    /** Override the dispatch context (defaults to a command-tier test epoch). */
    context?: Context;
}

export interface TestHost {
    initialize(overrides?: Partial<InitializeParams>): Promise<InitializeResult>;
    callTool(tool: string, args: Record<string, unknown>, opts?: CallToolOptions): Promise<ToolExecuteResult>;
    /** Drive a `hook` request and get back the extension's folded outcome. */
    callHook(hook: string, input: Record<string, unknown>, context?: Context): Promise<HookOutcome>;
    ping(): Promise<Record<string, unknown>>;
    sendEvent(event: string, payload?: Record<string, unknown>, context?: Context): void;
    shutdown(): Promise<void>;
    close(): void;
}

const DEFAULT_CONTEXT: Context = { token: 'test-epoch', tier: 'command' };

export function createTestHost(extension: Extension): TestHost {
    const [hostT, extT] = linkedPair();
    const extHandle = extension.connect(extT);
    /** call_id → the caller's onUpdate, so streamed progress reaches the test. */
    const updateSinks = new Map<string, (u: ToolUpdateParams) => void>();

    const host = new Peer({ send: (frame) => hostT.send(frame) });
    host.setNotificationHandler(method.TOOL_UPDATE, (params) => {
        const p = params as ToolUpdateParams;
        updateSinks.get(p.call_id)?.(p);
    });
    // Extension notifications the host just observes in tests.
    host.setNotificationHandler(method.LOG, () => {});
    host.setNotificationHandler(method.REGISTRY_UPDATE, () => {});
    hostT.start((frame) => host.receive(frame));

    return {
        initialize(overrides) {
            const params: InitializeParams = {
                protocol_version: PROTOCOL_VERSION,
                host: { name: 'smooth-test-host', version: '0.0.0' },
                workspace: { root: process.cwd(), trusted: true },
                mode: 'headless',
                capabilities_enabled: { tools: true },
                ...overrides,
            };
            return host.request<InitializeResult>(method.INITIALIZE, params);
        },
        async callTool(tool, args, opts = {}) {
            const call_id = `test-call-${++callSeq}`;
            if (opts.onUpdate) updateSinks.set(call_id, opts.onUpdate);
            try {
                return await host.request<ToolExecuteResult>(
                    method.TOOL_EXECUTE,
                    { call_id, tool, arguments: args, context: opts.context ?? DEFAULT_CONTEXT },
                    opts.signal,
                );
            } finally {
                updateSinks.delete(call_id);
            }
        },
        callHook(hook, input, context) {
            return host.request<HookOutcome>(method.HOOK, { hook, input, context: context ?? DEFAULT_CONTEXT });
        },
        ping() {
            return host.request<Record<string, unknown>>(method.PING, {});
        },
        sendEvent(event, payload, context) {
            host.notify(method.EVENT, { event, context: context ?? { token: DEFAULT_CONTEXT.token, tier: 'event' }, ...(payload ? { payload } : {}) });
        },
        async shutdown() {
            await host.request(method.SHUTDOWN, {});
        },
        close() {
            extHandle.close();
            host.close();
            hostT.close();
        },
    };
}
