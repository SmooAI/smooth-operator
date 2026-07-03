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
    SESSION_SET_MODEL: 'session/set_model',
    PROVIDER_COMPLETE: 'provider/complete',
    PROVIDER_DELTA: 'provider/delta',
    PROVIDER_OAUTH_LOGIN: 'provider/oauth_login',
    PROVIDER_OAUTH_REFRESH: 'provider/oauth_refresh',
    /** Ext → host: publish onto the inter-extension bus (Phase 8). */
    BUS_PUBLISH: 'bus/publish',
} as const;

/** SEP observe-event names the host fans out (Phase 8 additions included). */
export const eventName = {
    /** Inter-extension bus fanout — payload `{ from, topic, payload }`. */
    BUS_EVENT: 'bus/event',
    /** Targeted render-block v2 keypress — payload `{ widget_id?, key }`. */
    WIDGET_KEY: 'widget/key',
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

export interface ProviderModel {
    /** Model id the host passes back in `provider/complete`. */
    id: string;
    /** Human-facing label for model pickers. */
    display_name?: string;
}

export interface ProviderRegistration {
    /** Provider name, unique within the host's merged model surface. */
    name: string;
    /** Informational upstream base URL (the extension does the real call). */
    base_url?: string;
    /** Env var the extension reads its API key from — informational to the host. */
    api_key_env?: string;
    /** Whether the extension implements `provider/oauth_login` + `oauth_refresh`. */
    oauth?: boolean;
    models?: ProviderModel[];
}

/** A declarative message renderer (Phase 8, pi's `registerMessageRenderer`):
 *  a custom message `tag` → render-block `template`. When a session entry carries
 *  the tag, the frontend renders the template with `{{path}}` placeholders
 *  resolved against the entry's data. Data-only — the host never runs it. */
export interface MessageRendererRegistration {
    tag: string;
    template: RenderBlock;
}

export interface Registrations {
    tools?: ToolRegistration[];
    commands?: CommandRegistration[];
    flags?: string[];
    shortcuts?: ShortcutRegistration[];
    subscriptions?: string[];
    providers?: ProviderRegistration[];
    /** Intercept hooks this extension handles (Phase 8) — lets the host skip the
     *  per-turn `context` hook when no extension handles it. Empty = unknown. */
    hooks?: string[];
    /** Declarative custom-message renderers (Phase 8). */
    message_renderers?: MessageRendererRegistration[];
}

// --- render blocks (Phase 8) --------------------------------------------

/** One key an interactive `widget` render block declares. The host routes the
 *  matching keypress back as a `widget/key` event ({@link WidgetKeyPayload}). */
export interface Keybinding {
    /** A human chord the frontend matches, e.g. `ArrowUp`, `space`, `q`. */
    key: string;
    description?: string;
}

/**
 * The declarative render-block DSL (Phase 8) — replaces pi's function renderers.
 * The host/frontend renders each `kind` natively (TUI/web/widget); `text` is the
 * always-available plain fallback (frontends may derive one when omitted). The
 * `widget` kind is the interactive tier: it wraps a `body` block and declares
 * `keybindings`; the host routes matching keys back as `widget/key` events and
 * the extension re-renders via `ui.setWidget`.
 */
export type RenderBlock =
    | { kind: 'markdown'; text: string }
    | { kind: 'keyvalue'; rows: { key: string; value: string }[]; title?: string; text?: string }
    | { kind: 'table'; columns: string[]; rows: string[][]; text?: string }
    | { kind: 'diff'; patch: string; text?: string }
    | { kind: 'progress'; value: number; label?: string; text?: string }
    | { kind: 'stack'; children: RenderBlock[]; text?: string }
    | { kind: 'widget'; widget_id: string; body: RenderBlock; keybindings: Keybinding[]; text?: string };

/** Payload of the `bus/event` observe event (inter-extension bus, Phase 8). */
export interface BusEventPayload {
    from: string;
    topic: string;
    payload?: unknown;
}

/** Payload of the `widget/key` observe event (render-block v2, Phase 8). */
export interface WidgetKeyPayload {
    widget_id?: string;
    key: string;
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
    | { kind: 'set_widget'; widget: RenderBlock }
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

export interface SessionSetModelParams {
    context: Context;
    model: string;
    /** Provider name when the model belongs to an extension-registered provider. */
    provider?: string;
    /** Reasoning/thinking level, e.g. `off`, `low`, `medium`, `high`. */
    thinking?: string;
}

// --- provider/* (Phase 7) ------------------------------------------------

/** A serialized host stream event, tagged by `type`. Emitted as `provider/delta`
 *  `event` while a streaming `provider/complete` runs. */
export type ProviderStreamEvent =
    | { type: 'Delta'; content: string }
    | { type: 'Reasoning'; content: string }
    | { type: 'ToolCallStart'; index: number; id: string; name: string }
    | { type: 'ToolCallArgumentsDelta'; index: number; arguments_chunk: string }
    | { type: 'Usage'; prompt_tokens?: number; completion_tokens?: number; total_tokens?: number; cached_tokens?: number }
    | { type: 'Model'; name: string }
    | { type: 'Done'; finish_reason: string };

/** Host → ext `provider/complete`: run one completion. `messages`/`tools` are the
 *  host's opaque serialized shapes. */
export interface ProviderCompleteParams {
    request_id: string;
    provider: string;
    model: string;
    messages: Record<string, unknown>[];
    tools?: Record<string, unknown>[];
    stream?: boolean;
    response_format?: Record<string, unknown>;
    thinking?: string;
    context?: Context;
}

export interface ProviderToolCall {
    id: string;
    name: string;
    arguments: unknown;
}

export interface ProviderUsage {
    prompt_tokens?: number;
    completion_tokens?: number;
    total_tokens?: number;
    cached_tokens?: number;
}

/** The final reply to `provider/complete`, mapping onto the host's LlmResponse. */
export interface ProviderCompleteResult {
    content?: string;
    tool_calls?: ProviderToolCall[];
    finish_reason?: string;
    usage?: ProviderUsage;
    reasoning_content?: string;
    resolved_model?: string;
}

/** Ext → host `provider/delta` notification: one streamed chunk. */
export interface ProviderDeltaParams {
    request_id: string;
    event: ProviderStreamEvent;
}

/** Host → ext `provider/oauth_login` / `provider/oauth_refresh`. */
export interface ProviderOAuthParams {
    provider: string;
    refresh_token?: string;
    context?: Context;
}

export interface ProviderCredentials {
    api_key?: string;
    access_token?: string;
    refresh_token?: string;
    expires_at?: number;
    extra?: Record<string, unknown>;
}
