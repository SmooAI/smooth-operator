/**
 * SEP extension hosting for the TypeScript operator server.
 *
 * Wires the engine's `ExtensionHost` (`@smooai/smooth-operator-core/extension`)
 * into a turn so a server-side agent can host extensions: discover `extension.toml`
 * extensions, spawn them as JSON-RPC/ndjson subprocesses, and register their tools
 * into the turn's tool set. The TS sibling of the Rust server's `extensions.rs`.
 *
 * ## Trust — default deny
 * The server has no interactive trust prompt (a multi-session service can't stop to
 * ask a human). `SMOOTH_EXTENSIONS_ALLOW` (comma-separated extension names) IS the
 * trust decision: empty (the default) ⇒ **no extension is ever spawned** and the
 * host is never built, so behavior is byte-for-byte unchanged.
 *
 * ## `ui/confirm` → the existing confirmation frame
 * {@link ConfirmUiProvider} projects an extension's `ui/confirm` onto the protocol's
 * `write_confirmation_required` / `confirm_tool_action` frames — the same
 * session-keyed bridge the native write-tool HITL uses ({@link ConfirmationRegistry}):
 * register a resolver under the session, emit the frame, and park the extension's
 * request until the client answers. Every other `ui/*` degrades headless
 * (interactive → `{cancelled}`, render-only → `{}`); we advertise only `confirm` at
 * handshake so a well-behaved extension gates the rest off via `hasUI`.
 */
import {
    DefaultHostDelegate,
    defaultGlobalDir,
    discover,
    type DiscoveredExtension,
    ExtensionHost,
    type HostInfo,
    projectDir,
    type WorkspaceInfo,
} from '@smooai/smooth-operator-core/extension';

import type { ConfirmationRegistry } from './confirmation.js';
import * as protocol from './protocol.js';
import type { Sink } from './turnRunner.js';

/** Frontend `mode` announced to extensions — the server fronts the chat widget. */
const UI_MODE = 'widget';

/**
 * How long a parked `ui/confirm` waits for the client's `confirm_tool_action`
 * before the bridge resolves it as cancelled. Matches the Rust reference so an
 * abandoned confirm always settles (fail-closed) instead of hanging.
 */
const UI_CONFIRM_TIMEOUT_MS = 300_000;

/** Package version reported to extensions at handshake (best-effort). */
const HOST_VERSION = process.env.npm_package_version ?? '0.0.0';

/** Parse `SMOOTH_EXTENSIONS_ALLOW` into allowed names (comma-separated, trimmed, empties dropped). */
export function parseAllowlist(raw: string | undefined): string[] {
    return (raw ?? '')
        .split(',')
        .map((s) => s.trim())
        .filter((s) => s.length > 0);
}

/** What {@link buildExtensionHost} needs from the turn to route a `ui/confirm` back. */
export interface ExtensionTurnContext {
    /** The session-keyed pending-confirmation registry (shared with the dispatcher). */
    confirmations?: ConfirmationRegistry;
    /** The session a confirmation is registered under. */
    sessionId: string;
    /** The protocol request id (streaming correlation on the confirmation frame). */
    requestId: string;
    /** The turn's protocol sink — where `write_confirmation_required` goes. */
    sink: Sink;
}

/**
 * The {@link HostDelegate} that bridges `ui/confirm` onto the confirmation frame and
 * degrades every other `ui/*` headless. Bound to ONE turn (its sink, request id,
 * session), which is why the host is built per turn — a shared host could not route
 * a `ui/*` back to the right session's socket.
 */
export class ConfirmUiProvider extends DefaultHostDelegate {
    constructor(private readonly ctx: ExtensionTurnContext) {
        super();
    }

    override async uiRequest(ext: string, params: unknown): Promise<unknown> {
        const p = (params ?? {}) as { kind?: string; prompt?: string };
        switch (p.kind) {
            case 'confirm': {
                // No registry ⇒ nothing can answer; degrade to a dismissed dialog.
                if (!this.ctx.confirmations) return { cancelled: true };
                const prompt = typeof p.prompt === 'string' ? p.prompt : 'Confirm this action?';
                // Register a fresh resolver for this session so the next inbound
                // `confirm_tool_action` resumes THIS request, then emit the frame and
                // park until the human answers (or the turn ends and it resolves false).
                const verdict = this.ctx.confirmations.register(this.ctx.sessionId);
                this.ctx.sink(protocol.writeConfirmationRequired(this.ctx.requestId, ext, prompt));
                // Race the client's verdict against a timeout so an abandoned confirm
                // always settles (fail-closed → cancelled) instead of hanging. The timer
                // is unref'd so it never keeps the process alive on its own.
                const approved = await Promise.race([
                    verdict,
                    new Promise<boolean>((resolve) => {
                        setTimeout(() => resolve(false), UI_CONFIRM_TIMEOUT_MS).unref?.();
                    }),
                ]);
                return approved ? { confirmed: true } : { cancelled: true };
            }
            // Render-only kinds: accept and drop — there's no chat frame for them.
            case 'notify':
            case 'set_status':
            case 'set_widget':
            case 'set_title':
                return {};
            // select/input need an answer we can't source from a confirm button.
            default:
                return { cancelled: true };
        }
    }
}

/**
 * Discover, trust-gate (allowlist), and load the per-turn extension host. Returns
 * `undefined` — the host is never built, zero overhead — when the allowlist is empty
 * (default deny) or no allowed extension loads.
 *
 * The caller registers the returned host's `tools()` into the turn's tool set (so
 * they flow through the same `enabled_tools` filter built-ins get) and calls
 * `shutdownAll()` at turn end.
 */
export async function buildExtensionHost(ctx: ExtensionTurnContext): Promise<ExtensionHost | undefined> {
    // Trust = a default-deny env allowlist (the server has no interactive prompt).
    const allow = parseAllowlist(process.env.SMOOTH_EXTENSIONS_ALLOW);
    if (allow.length === 0) return undefined; // default deny — never spawn anything

    // `SMOOTH_EXTENSIONS_DIR` overrides the discovery dir; else the engine default.
    const globalDir = process.env.SMOOTH_EXTENSIONS_DIR?.trim() || defaultGlobalDir();
    // The server has no per-session workspace; project-scoped discovery keys off the
    // process cwd's `.smooth/extensions`. Usually absent → global only.
    const project = projectDir(process.cwd());
    const { extensions, failures } = discover(globalDir, project);
    for (const [src, err] of failures) console.warn(`[sep] extension manifest failed to parse: ${src}: ${err}`);

    const allowed: DiscoveredExtension[] = extensions.filter((ext) => {
        const ok = allow.includes(ext.manifest.name);
        if (!ok) console.info(`[sep] skipping extension not in SMOOTH_EXTENSIONS_ALLOW: ${ext.manifest.name}`);
        return ok;
    });
    if (allowed.length === 0) return undefined;

    const host: HostInfo = { name: 'smooth-operator-server', version: HOST_VERSION };
    // Allowlisted ⇒ trusted (the allowlist is the trust decision); project-scoped
    // extensions load because `trusted` is true.
    const workspace: WorkspaceInfo = { root: process.cwd(), trusted: true };
    const delegate = new ConfirmUiProvider(ctx);

    const { host: extHost, failures: loadFailures } = await ExtensionHost.load(allowed, host, workspace, UI_MODE, ['confirm'], delegate);
    for (const [name, err] of loadFailures) console.warn(`[sep] extension failed to load: ${name}: ${err}`);
    if (extHost.isEmpty()) return undefined;
    console.info(`[sep] attached extension host to the turn: ${extHost.names().join(', ')}`);
    return extHost;
}
