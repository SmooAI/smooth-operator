/**
 * SEP (Smooth Extension Protocol) wire types + method/error constants.
 *
 * The source of truth is the JSON Schemas in `spec/extension/`; these types are
 * the hand-maintained TS view of the subset the SDK needs for Phase 1 (the tool
 * path + lifecycle). Field names are `snake_case` because they ARE the wire.
 */

/** Highest SEP version this SDK speaks. Effective version = min(host, ext). */
export const PROTOCOL_VERSION = 1;

/** Method names — namespaced with `/`; `$/` marks JSON-RPC meta methods. */
export const method = {
    INITIALIZE: 'initialize',
    SHUTDOWN: 'shutdown',
    PING: 'ping',
    EVENT: 'event',
    HOOK: 'hook',
    TOOL_EXECUTE: 'tool/execute',
    TOOL_UPDATE: 'tool/update',
    UI_REQUEST: 'ui/request',
    REGISTRY_UPDATE: 'registry/update',
    LOG: 'log',
    CANCEL: '$/cancel',
    COMMAND_EXECUTE: 'command/execute',
    COMMAND_COMPLETE: 'command/complete',
    SESSION_SEND_MESSAGE: 'session/send_message',
    SESSION_SEND_USER_MESSAGE: 'session/send_user_message',
    SESSION_APPEND_ENTRY: 'session/append_entry',
} as const;

/** JSON-RPC + SEP error codes (see spec/extension/envelope.md). */
export const errorCode = {
    ParseError: -32700,
    InvalidRequest: -32600,
    MethodNotFound: -32601,
    InvalidParams: -32602,
    InternalError: -32603,
    Blocked: -32000,
    NoUI: -32001,
    NotTrusted: -32002,
    ContextViolation: -32003,
    CapabilityDisabled: -32004,
    Cancelled: -32800,
} as const;

/** The `context` carried by every dispatched event/hook/tool/execute. */
export interface Context {
    token: string;
    tier: 'event' | 'command';
}

export interface HostInfo {
    name: string;
    version: string;
}

export interface Workspace {
    root: string;
    trusted: boolean;
}

export interface InitializeParams {
    protocol_version: number;
    host: HostInfo;
    workspace: Workspace;
    session?: { id?: string };
    mode: 'tui' | 'web' | 'widget' | 'cli' | 'headless';
    ui_capabilities?: string[];
    /** Parsed values for the flags the extension declares (name → value). */
    flags?: Record<string, unknown>;
    capabilities_enabled?: Record<string, boolean>;
}

export interface ToolRegistration {
    name: string;
    description: string;
    /** JSON Schema for the tool's arguments. */
    parameters: Record<string, unknown>;
    deferred?: boolean;
}

export interface CommandRegistration {
    name: string;
    description: string;
}

export interface ShortcutRegistration {
    /** A human-typed chord, e.g. `ctrl+p`; the frontend parses it. */
    key: string;
    /** The registered command this chord invokes (no leading `/`). */
    command: string;
    description?: string;
}

export interface Registrations {
    tools?: ToolRegistration[];
    commands?: CommandRegistration[];
    flags?: string[];
    shortcuts?: ShortcutRegistration[];
    subscriptions?: string[];
}

export interface InitializeResult {
    protocol_version: number;
    extension: { name: string; version: string };
    registrations?: Registrations;
}

export interface ToolExecuteParams {
    call_id: string;
    tool: string;
    arguments: Record<string, unknown>;
    context: Context;
}

export interface ToolExecuteResult {
    content: string;
    is_error?: boolean;
    details?: unknown;
}

export interface ToolUpdateParams {
    call_id: string;
    message?: string;
    progress?: number;
    details?: unknown;
}

export interface EventParams {
    event: string;
    /** Per-connection monotonic sequence; absent on the `events_lost` marker. */
    seq?: number;
    context: Context;
    payload?: Record<string, unknown>;
}

/** Host → ext `hook` request: an awaited intercept the extension can veto/patch. */
export interface HookParams {
    hook: string;
    context: Context;
    input: Record<string, unknown>;
}

/** The extension's reply to a `hook`, tagged by `action`. */
export type HookOutcome =
    | { action: 'continue' }
    | { action: 'block'; reason?: string }
    | { action: 'modify'; patch: Record<string, unknown> };

/** The seven `ui/request` kinds (snake_case wire names). */
export type UiKind = 'select' | 'confirm' | 'input' | 'notify' | 'set_status' | 'set_widget' | 'set_title';

/** Params of `ui/request` (ext → host), discriminated by `kind`. */
export type UiRequestParams =
    | { kind: 'select'; prompt: string; options: string[] }
    | { kind: 'confirm'; prompt: string }
    | { kind: 'input'; prompt: string; default?: string }
    | { kind: 'notify'; message: string; level?: 'info' | 'warn' | 'error' }
    | { kind: 'set_status'; status: string }
    | { kind: 'set_widget'; widget: Record<string, unknown> }
    | { kind: 'set_title'; title: string };

/**
 * Reply to a `ui/request`. Which field is set depends on the request `kind`:
 * `select` → `value`, `confirm` → `confirmed`, `input` → `text`; the rest are
 * empty. Any kind may set `cancelled` if the user dismissed the UI.
 */
export interface UiRequestResult {
    value?: string;
    confirmed?: boolean;
    text?: string;
    cancelled?: boolean;
}

/** Host → ext `command/execute`: run a registered slash-command (command tier). */
export interface CommandExecuteParams {
    command: string;
    context: Context;
    arguments?: Record<string, unknown>;
}

export interface CommandExecuteResult {
    content?: string;
}

/** Host → ext `command/complete`: argument autocomplete for a slash-command. */
export interface CommandCompleteParams {
    command: string;
    context: Context;
    partial?: string;
}

export interface Completion {
    value: string;
    description?: string;
}

export interface CommandCompleteResult {
    completions: Completion[];
}

/** How a `session/send_user_message` is delivered relative to the current turn. */
export type DeliverAs = 'steer' | 'follow_up' | 'next_turn';

/** Ext → host `session/send_message` params (command tier — carries `context`). */
export interface SessionSendMessageParams {
    context: Context;
    text: string;
    role?: 'user' | 'assistant';
}

export interface SessionSendUserMessageParams {
    context: Context;
    text: string;
    deliver_as?: DeliverAs;
}

export interface SessionAppendEntryParams {
    context: Context;
    entry: Record<string, unknown>;
}
