/**
 * `createTestHost` — an in-process scripted SEP host for unit-testing an
 * extension without spawning a subprocess. It plays the host side of the
 * protocol over a `linkedPair`, so tests drive `initialize`, `tool/execute`
 * (with progress + cancellation), events, ping and shutdown directly against a
 * `defineExtension(...)` object.
 */
import { Peer, RpcError } from './jsonrpc.js';
import { PROTOCOL_VERSION, errorCode, method } from './protocol.js';
import type {
    CommandExecuteResult,
    Context,
    HookOutcome,
    InitializeParams,
    InitializeResult,
    ToolExecuteResult,
    ToolUpdateParams,
    UiRequestParams,
    UiRequestResult,
} from './protocol.js';
import type { Extension } from './extension.js';
import { linkedPair } from './transport.js';

let callSeq = 0;

/** Answers the extension's `ui/request` calls. Return a result, or throw an
 * `RpcError` (e.g. code -32001) to simulate a headless/uncapable frontend. */
export type UiResponder = (params: UiRequestParams) => UiRequestResult | Promise<UiRequestResult>;

/** A recorded ext→host `session/*` request the test can assert on. */
export interface SessionCall {
    method: string;
    params: Record<string, unknown>;
}

export interface CreateTestHostOptions {
    /** Answers `ui/request`. Default: reject every call with -32001 NoUI. */
    onUiRequest?: UiResponder;
}

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
    /** Dispatch a `command/execute` with a command-tier context by default. */
    runCommand(command: string, args?: Record<string, unknown>, context?: Context): Promise<CommandExecuteResult>;
    /** Dispatch a `command/complete` for argument autocomplete. */
    completeCommand(command: string, partial: string, context?: Context): Promise<{ completions: { value: string; description?: string }[] }>;
    ping(): Promise<Record<string, unknown>>;
    sendEvent(event: string, payload?: Record<string, unknown>, context?: Context): void;
    /** Every `session/*` request the extension made, in order — for assertions. */
    readonly sessionCalls: SessionCall[];
    shutdown(): Promise<void>;
    close(): void;
}

const DEFAULT_CONTEXT: Context = { token: 'test-epoch', tier: 'command' };

export function createTestHost(extension: Extension, options: CreateTestHostOptions = {}): TestHost {
    const [hostT, extT] = linkedPair();
    const extHandle = extension.connect(extT);
    /** call_id → the caller's onUpdate, so streamed progress reaches the test. */
    const updateSinks = new Map<string, (u: ToolUpdateParams) => void>();

    const host = new Peer({ send: (frame) => hostT.send(frame) });
    host.setNotificationHandler(method.TOOL_UPDATE, (params) => {
        const p = params as ToolUpdateParams;
        updateSinks.get(p.call_id)?.(p);
    });
    // Answer ext→host `ui/request`. Default mimics a headless frontend (NoUI).
    host.setRequestHandler(method.UI_REQUEST, async (params) => {
        if (!options.onUiRequest) throw new RpcError(errorCode.NoUI, 'no UI available (headless test host)');
        return options.onUiRequest(params as UiRequestParams);
    });
    // Extension notifications the host just observes in tests.
    host.setNotificationHandler(method.LOG, () => {});
    host.setNotificationHandler(method.REGISTRY_UPDATE, () => {});

    // Service ext→host `session/*` requests, enforcing the same command-tier
    // guard the real host does (event-tier → -32003) so a demo's session calls
    // are exercised realistically. Every call is recorded for assertions.
    const sessionCalls: SessionCall[] = [];
    const sessionHandler = (params: unknown) => {
        const p = (params ?? {}) as Record<string, unknown>;
        const tier = (p.context as { tier?: string } | undefined)?.tier;
        if (tier !== 'command') throw new RpcError(errorCode.ContextViolation, 'session action requires a command-tier context');
        return p;
    };
    for (const m of [method.SESSION_SEND_MESSAGE, method.SESSION_SEND_USER_MESSAGE, method.SESSION_APPEND_ENTRY]) {
        host.setRequestHandler(m, (params) => {
            const recorded = sessionHandler(params);
            sessionCalls.push({ method: m, params: recorded });
            return {};
        });
    }
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
        runCommand(command, args, context) {
            return host.request<CommandExecuteResult>(method.COMMAND_EXECUTE, {
                command,
                context: context ?? DEFAULT_CONTEXT,
                ...(args ? { arguments: args } : {}),
            });
        },
        completeCommand(command, partial, context) {
            return host.request(method.COMMAND_COMPLETE, { command, context: context ?? DEFAULT_CONTEXT, partial });
        },
        sessionCalls,
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
