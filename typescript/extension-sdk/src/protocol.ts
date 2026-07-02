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
    REGISTRY_UPDATE: 'registry/update',
    LOG: 'log',
    CANCEL: '$/cancel',
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

export interface Registrations {
    tools?: ToolRegistration[];
    commands?: CommandRegistration[];
    flags?: string[];
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
    context: Context;
    payload?: Record<string, unknown>;
}
