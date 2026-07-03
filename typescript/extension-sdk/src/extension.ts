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
import type {
    CommandCompleteParams,
    CommandCompleteResult,
    CommandExecuteParams,
    CommandExecuteResult,
    CommandRegistration,
    Completion,
    Context,
    DeliverAs,
    EventParams,
    HookOutcome,
    HookParams,
    InitializeParams,
    InitializeResult,
    ShortcutRegistration,
    ToolExecuteParams,
    ToolExecuteResult,
    ToolUpdateParams,
    UiKind,
    UiRequestParams,
    UiRequestResult,
} from './protocol.js';
import { toJsonSchema, type ParameterSchema } from './schema.js';
import { stdioTransport, type Transport } from './transport.js';

/** The intercept hooks (awaited, host-orchestrated); every other `on(...)` name
 *  is a fire-and-forget observe event. Kept in sync with the engine's HookType. */
const HOOK_NAMES = new Set(['tool_call', 'tool_result', 'before_agent_start', 'message_end', 'context', 'before_provider_request', 'input', 'user_bash']);

/**
 * The `ui/request` surface handed to tools (and to event handlers via
 * `smooth.ui`). Each call is an ext→host request; the frontend renders it and
 * replies. `select`/`confirm`/`input` return an answer (or `{ cancelled: true }`
 * if dismissed); `notify`/`setStatus`/`setWidget`/`setTitle` resolve empty. A
 * headless or uncapable host rejects with an `RpcError` of code -32001 (NoUI) —
 * gate with `hasUI(kind)` to avoid it.
 */
export interface UiApi {
    select(prompt: string, options: string[]): Promise<UiRequestResult>;
    confirm(prompt: string): Promise<UiRequestResult>;
    input(prompt: string, opts?: { default?: string }): Promise<UiRequestResult>;
    notify(message: string, level?: 'info' | 'warn' | 'error'): Promise<void>;
    setStatus(status: string): Promise<void>;
    setWidget(widget: Record<string, unknown>): Promise<void>;
    setTitle(title: string): Promise<void>;
}

/** Build a [`UiApi`] that speaks `ui/request` over `peer`. */
function makeUi(peer: Peer): UiApi {
    const req = (params: UiRequestParams) => peer.request<UiRequestResult>(method.UI_REQUEST, params);
    return {
        select: (prompt, options) => req({ kind: 'select', prompt, options }),
        confirm: (prompt) => req({ kind: 'confirm', prompt }),
        input: (prompt, opts) => req({ kind: 'input', prompt, ...(opts?.default !== undefined ? { default: opts.default } : {}) }),
        notify: async (message, level) => {
            await req({ kind: 'notify', message, ...(level ? { level } : {}) });
        },
        setStatus: async (status) => {
            await req({ kind: 'set_status', status });
        },
        setWidget: async (widget) => {
            await req({ kind: 'set_widget', widget });
        },
        setTitle: async (title) => {
            await req({ kind: 'set_title', title });
        },
    };
}

/**
 * Session-mutating ext→host actions. Available only from a COMMAND-tier context
 * (command handlers) — the host rejects them from an event-tier context with
 * -32003 ContextViolation. `sendMessage` posts a message, `sendUserMessage`
 * delivers a user message (steer/follow_up/next_turn), `appendEntry` persists an
 * LLM-invisible transcript entry.
 */
export interface SessionApi {
    sendMessage(text: string, opts?: { role?: 'user' | 'assistant' }): Promise<void>;
    sendUserMessage(text: string, opts?: { deliverAs?: DeliverAs }): Promise<void>;
    appendEntry(entry: Record<string, unknown>): Promise<void>;
}

/** Build a [`SessionApi`] bound to `context` (must be command-tier) over `peer`. */
function makeSession(peer: Peer, context: Context): SessionApi {
    return {
        sendMessage: async (text, opts) => {
            await peer.request(method.SESSION_SEND_MESSAGE, { context, text, ...(opts?.role ? { role: opts.role } : {}) });
        },
        sendUserMessage: async (text, opts) => {
            await peer.request(method.SESSION_SEND_USER_MESSAGE, { context, text, ...(opts?.deliverAs ? { deliver_as: opts.deliverAs } : {}) });
        },
        appendEntry: async (entry) => {
            await peer.request(method.SESSION_APPEND_ENTRY, { context, entry });
        },
    };
}

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
    /** Ask the frontend to render a dialog/widget. See [`UiApi`]. */
    ui: UiApi;
    /** True if the host's frontend can render this `ui/request` kind. */
    hasUI(kind: UiKind): boolean;
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

/** What a command handler receives: the command-tier context plus the session,
 *  ui, and args bound to it. Session actions are valid because a command runs at
 *  command tier. */
export interface CommandContext {
    /** The dispatch context (command tier). */
    context: Context;
    /** Free-form arguments parsed from the invocation. */
    args: Record<string, unknown> | undefined;
    /** Session-mutating actions, bound to this command's context. */
    session: SessionApi;
    /** Ask the frontend to render a dialog/widget. See [`UiApi`]. */
    ui: UiApi;
    /** True if the host's frontend can render this `ui/request` kind. */
    hasUI(kind: UiKind): boolean;
    /** Structured log line into host tracing. */
    log(level: 'debug' | 'info' | 'warn' | 'error', message: string, fields?: Record<string, unknown>): void;
}

/** What a command's `execute` may return: text to surface, a full result, or
 *  nothing. */
export type CommandReturn = CommandExecuteResult | string | void;

export interface CommandDef {
    name: string;
    description: string;
    execute(ctx: CommandContext): Promise<CommandReturn> | CommandReturn;
    /** Optional argument autocomplete: given the partial text, return candidates. */
    complete?(partial: string, context: Context): Promise<Completion[]> | Completion[];
}

/** Identity helper for `registerCommand` call sites. */
export function defineCommand(def: CommandDef): CommandDef {
    return def;
}

/** A CLI/slash flag the extension declares. The host delivers its parsed value
 *  in `initialize`; read it with `smooth.getFlag(name)`. */
export interface FlagDef {
    name: string;
    description?: string;
}

/** A hook handler's friendly return: veto the operation, or replace its input
 *  with a patch (shallow-merged onto the input). Returning nothing = continue. */
export type HookResult = { block: true; reason?: string } | { patch: Record<string, unknown> };

/** Handler for an `on(name, ...)` registration. For an observe event the return
 *  is ignored; for an intercept hook (see {@link HOOK_NAMES}) return a
 *  {@link HookResult} to veto or patch. Mirrors pi's single `on`. */
export type EventHandler = (data: Record<string, unknown> | undefined, ctx: Context) => void | HookResult | Promise<void | HookResult>;

/** The builder passed to `defineExtension`'s setup. Mirrors pi's `ExtensionAPI`. */
export interface SmoothApi {
    name: string;
    version: string;
    registerTool(tool: ToolDef<any>): void;
    /** Register a slash-command surfaced in the host's `/` palette. */
    registerCommand(command: CommandDef): void;
    /** Declare a CLI/slash flag; read its delivered value with [`getFlag`]. */
    registerFlag(flag: FlagDef): void;
    /** Bind a keyboard shortcut (TUI frontends) to a registered command. */
    registerShortcut(shortcut: ShortcutRegistration): void;
    on(event: string, handler: EventHandler): void;
    log(level: 'debug' | 'info' | 'warn' | 'error', message: string, fields?: Record<string, unknown>): void;
    /** The parsed value the host delivered for a declared flag (undefined before
     * `initialize`, or when the host has no CLI surface). */
    getFlag(name: string): unknown;
    /** True if the host's frontend can render this `ui/request` kind. Only
     * meaningful after `initialize` — returns false before the handshake. */
    hasUI(kind: UiKind): boolean;
    /** The `ui/request` surface. Available after the extension connects; throws
     * if read before then. Gate with [`hasUI`]. */
    readonly ui: UiApi;
}

export type ExtensionSetup = (smooth: SmoothApi) => void;

export interface ConnectHandle {
    peer: Peer;
    close(): void;
}

export class Extension {
    private readonly tools = new Map<string, ToolDef<any>>();
    private readonly commands = new Map<string, CommandDef>();
    private readonly flagDefs = new Map<string, FlagDef>();
    private readonly shortcuts: ShortcutRegistration[] = [];
    private readonly events = new Map<string, EventHandler[]>();
    private name = 'extension';
    private version = '0.0.0';
    /** Set once connected so `log()` before connect is a safe no-op. */
    private live?: Peer;
    /** UI kinds the host declared answerable at `initialize`. */
    private hostUiCaps: string[] = [];
    /** Flag values the host delivered at `initialize` (name → value). */
    private flagValues: Record<string, unknown> = {};

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
            registerCommand: (command) => {
                this.commands.set(command.name, command);
            },
            registerFlag: (flag) => {
                this.flagDefs.set(flag.name, flag);
            },
            registerShortcut: (shortcut) => {
                this.shortcuts.push(shortcut);
            },
            on: (event, handler) => {
                const list = this.events.get(event) ?? [];
                list.push(handler);
                this.events.set(event, list);
            },
            log: (level, message, fields) => {
                this.live?.notify(method.LOG, { level, message, ...(fields ? { fields } : {}) });
            },
            getFlag: (name) => self.flagValues[name],
            hasUI: (kind) => self.hostUiCaps.includes(kind),
            get ui() {
                if (!self.live) throw new Error('smooth.ui is only available after the extension connects');
                return makeUi(self.live);
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
        peer.setRequestHandler(method.HOOK, (params) => this.dispatchHook(params as HookParams));
        peer.setRequestHandler(method.COMMAND_EXECUTE, (params) => this.executeCommand(params as CommandExecuteParams, peer));
        peer.setRequestHandler(method.COMMAND_COMPLETE, (params) => this.completeCommand(params as CommandCompleteParams));
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

    private initialize(params: InitializeParams): InitializeResult {
        this.hostUiCaps = params.ui_capabilities ?? [];
        this.flagValues = params.flags ?? {};
        const tools = [...this.tools.values()].map((t) => ({
            name: t.name,
            description: t.description,
            parameters: toJsonSchema(t.parameters),
            ...(t.deferred ? { deferred: true } : {}),
        }));
        const commands: CommandRegistration[] = [...this.commands.values()].map((c) => ({ name: c.name, description: c.description }));
        const flags = [...this.flagDefs.keys()];
        // Only observe events go in `subscriptions` — hook names are intercepts
        // the host always calls, not events it filters by subscription.
        const subscriptions = [...this.events.keys()].filter((name) => !HOOK_NAMES.has(name));
        return {
            protocol_version: PROTOCOL_VERSION,
            extension: { name: this.name, version: this.version },
            registrations: {
                tools,
                ...(commands.length ? { commands } : {}),
                ...(flags.length ? { flags } : {}),
                ...(this.shortcuts.length ? { shortcuts: this.shortcuts } : {}),
                subscriptions,
            },
        };
    }

    private async executeCommand(params: CommandExecuteParams, peer: Peer): Promise<CommandExecuteResult> {
        const command = this.commands.get(params.command);
        if (!command) return { content: `unknown command: ${params.command}` };
        const ctx: CommandContext = {
            context: params.context,
            args: params.arguments,
            session: makeSession(peer, params.context),
            ui: makeUi(peer),
            hasUI: (kind) => this.hostUiCaps.includes(kind),
            log: (level, message, fields) => peer.notify(method.LOG, { level, message, ...(fields ? { fields } : {}) }),
        };
        const out = await command.execute(ctx);
        if (out === undefined) return {};
        return typeof out === 'string' ? { content: out } : out;
    }

    private async completeCommand(params: CommandCompleteParams): Promise<CommandCompleteResult> {
        const command = this.commands.get(params.command);
        if (!command?.complete) return { completions: [] };
        const completions = await command.complete(params.partial ?? '', params.context);
        return { completions };
    }

    private async executeTool(params: ToolExecuteParams, peer: Peer, signal: AbortSignal): Promise<ToolExecuteResult> {
        const tool = this.tools.get(params.tool);
        if (!tool) return { content: `unknown tool: ${params.tool}`, is_error: true };
        const ctx: ToolContext = {
            callId: params.call_id,
            context: params.context,
            signal,
            onUpdate: (update) => peer.notify(method.TOOL_UPDATE, { call_id: params.call_id, ...update }),
            ui: makeUi(peer),
            hasUI: (kind) => this.hostUiCaps.includes(kind),
        };
        const out = await tool.execute(params.arguments, ctx);
        return typeof out === 'string' ? { content: out } : out;
    }

    private dispatchEvent(params: EventParams): void {
        for (const handler of this.events.get(params.event) ?? []) {
            void handler(params.payload, params.context);
        }
    }

    /** Fold this extension's handlers for one `hook` into a single outcome: the
     *  first `block` short-circuits; `patch`es shallow-merge onto the input and
     *  thread to the next handler; no result = continue. The host chains the
     *  outcome across extensions in load order. */
    private async dispatchHook(params: HookParams): Promise<HookOutcome> {
        let input = params.input;
        let modified = false;
        for (const handler of this.events.get(params.hook) ?? []) {
            const result = await handler(input, params.context);
            if (!result) continue;
            if ('block' in result) return { action: 'block', ...(result.reason ? { reason: result.reason } : {}) };
            input = { ...input, ...result.patch };
            modified = true;
        }
        return modified ? { action: 'modify', patch: input } : { action: 'continue' };
    }
}

/** Define an extension. Set `smooth.name`/`smooth.version` and register tools. */
export function defineExtension(setup: ExtensionSetup): Extension {
    return new Extension(setup);
}
