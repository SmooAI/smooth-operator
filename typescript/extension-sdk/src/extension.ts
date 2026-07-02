/**
 * `defineExtension` / `defineTool` — the DX centerpiece.
 *
 * An extension is a long-lived subprocess speaking SEP over its stdio. You
 * describe it declaratively; `serve()` wires it to `process.stdin/stdout`, and
 * `createTestHost` (test-host.ts) drives the same object in-process.
 *
 * Phase 1 surface: `registerTool` (schema + execute + streaming progress +
 * cancellation) and `on(event)` observe subscriptions. Hooks, commands, ui/kv/
 * session/exec land in later phases; the wire and this API were shaped to grow
 * into them without breaking the tool path.
 */
import { Peer } from './jsonrpc.js';
import { PROTOCOL_VERSION, method } from './protocol.js';
import type { Context, EventParams, InitializeParams, InitializeResult, ToolExecuteParams, ToolExecuteResult, ToolUpdateParams } from './protocol.js';
import { toJsonSchema, type ParameterSchema } from './schema.js';
import { stdioTransport, type Transport } from './transport.js';

/** Progress + cancellation handed to a tool while it runs. */
export interface ToolContext {
    /** Correlates `onUpdate` calls with this execution. */
    callId: string;
    /** The dispatch context (epoch token + tier). */
    context: Context;
    /** Fires when the host sends `$/cancel` for this call. */
    signal: AbortSignal;
    /** Stream a progress notification back to the host. */
    onUpdate(update: Omit<ToolUpdateParams, 'call_id'>): void;
}

/** What a tool's `execute` may return: a full result or just its `content`. */
export type ToolReturn = ToolExecuteResult | string;

export interface ToolDef<TArgs = Record<string, unknown>> {
    name: string;
    description: string;
    parameters: ParameterSchema;
    deferred?: boolean;
    execute(args: TArgs, ctx: ToolContext): Promise<ToolReturn> | ToolReturn;
}

/** Identity for `defineTool` — keeps the generic arg inferred at the call site. */
export function defineTool<TArgs = Record<string, unknown>>(def: ToolDef<TArgs>): ToolDef<TArgs> {
    return def;
}

/** Handler for an observe `event`. Fire-and-forget; return value ignored. */
export type EventHandler = (payload: Record<string, unknown> | undefined, ctx: Context) => void | Promise<void>;

/** The builder passed to `defineExtension`'s setup. Mirrors pi's `ExtensionAPI`. */
export interface SmoothApi {
    name: string;
    version: string;
    registerTool(tool: ToolDef<any>): void;
    on(event: string, handler: EventHandler): void;
    log(level: 'debug' | 'info' | 'warn' | 'error', message: string, fields?: Record<string, unknown>): void;
}

export type ExtensionSetup = (smooth: SmoothApi) => void;

export interface ConnectHandle {
    peer: Peer;
    close(): void;
}

export class Extension {
    private readonly tools = new Map<string, ToolDef<any>>();
    private readonly events = new Map<string, EventHandler[]>();
    private name = 'extension';
    private version = '0.0.0';
    /** Set once connected so `log()` before connect is a safe no-op. */
    private live?: Peer;

    constructor(setup: ExtensionSetup) {
        const api: SmoothApi = {
            get name() {
                return self.name;
            },
            set name(v: string) {
                self.name = v;
            },
            get version() {
                return self.version;
            },
            set version(v: string) {
                self.version = v;
            },
            registerTool: (tool) => {
                this.tools.set(tool.name, tool);
            },
            on: (event, handler) => {
                const list = this.events.get(event) ?? [];
                list.push(handler);
                this.events.set(event, list);
            },
            log: (level, message, fields) => {
                this.live?.notify(method.LOG, { level, message, ...(fields ? { fields } : {}) });
            },
        };
        // `self` alias so the getter/setter pair above closes over the instance.
        const self = this;
        setup(api);
    }

    /** Wire this extension to a transport. Returns a handle to close it. */
    connect(transport: Transport, onShutdown: () => void = () => {}): ConnectHandle {
        const peer = new Peer({ send: (frame) => transport.send(frame) });
        this.live = peer;

        peer.setRequestHandler(method.INITIALIZE, (params) => this.initialize(params as InitializeParams));
        peer.setRequestHandler(method.PING, () => ({}));
        peer.setRequestHandler(method.SHUTDOWN, () => {
            queueMicrotask(onShutdown);
            return {};
        });
        peer.setRequestHandler(method.TOOL_EXECUTE, (params, signal) => this.executeTool(params as ToolExecuteParams, peer, signal));
        peer.setNotificationHandler(method.EVENT, (params) => this.dispatchEvent(params as EventParams));

        transport.start((frame) => peer.receive(frame));
        return {
            peer,
            close() {
                peer.close();
                transport.close();
            },
        };
    }

    /** Connect over this process's stdin/stdout and keep the process alive. */
    serve(): ConnectHandle {
        return this.connect(stdioTransport(), () => {
            // Give the shutdown reply a tick to flush, then exit.
            setTimeout(() => process.exit(0), 10);
        });
    }

    private initialize(_params: InitializeParams): InitializeResult {
        const tools = [...this.tools.values()].map((t) => ({
            name: t.name,
            description: t.description,
            parameters: toJsonSchema(t.parameters),
            ...(t.deferred ? { deferred: true } : {}),
        }));
        return {
            protocol_version: PROTOCOL_VERSION,
            extension: { name: this.name, version: this.version },
            registrations: { tools, subscriptions: [...this.events.keys()] },
        };
    }

    private async executeTool(params: ToolExecuteParams, peer: Peer, signal: AbortSignal): Promise<ToolExecuteResult> {
        const tool = this.tools.get(params.tool);
        if (!tool) return { content: `unknown tool: ${params.tool}`, is_error: true };
        const ctx: ToolContext = {
            callId: params.call_id,
            context: params.context,
            signal,
            onUpdate: (update) => peer.notify(method.TOOL_UPDATE, { call_id: params.call_id, ...update }),
        };
        const out = await tool.execute(params.arguments, ctx);
        return typeof out === 'string' ? { content: out } : out;
    }

    private dispatchEvent(params: EventParams): void {
        for (const handler of this.events.get(params.event) ?? []) {
            void handler(params.payload, params.context);
        }
    }
}

/** Define an extension. Set `smooth.name`/`smooth.version` and register tools. */
export function defineExtension(setup: ExtensionSetup): Extension {
    return new Extension(setup);
}
