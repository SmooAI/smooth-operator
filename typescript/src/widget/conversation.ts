/**
 * ConversationController — the bridge between the widget UI and the
 * `@smooai/smooth-operator` protocol client.
 *
 * This is the piece that was rewired: the original smooai widget spoke to
 * `@smooai/realtime`; here every protocol action goes through {@link SmoothAgentClient}.
 * The wire shapes are identical (the protocol was lifted from `@smooai/realtime`),
 * so the swap is purely at the client-library boundary.
 *
 * Flow:
 *   1. `connect()`        → opens the WebSocket transport and `create_conversation_session`.
 *   2. `send(text)`       → `send_message`, streaming `stream_token` deltas into the
 *                           in-progress assistant message, then the terminal
 *                           `eventual_response`.
 *
 * The controller is UI-agnostic: it emits typed events and the view renders them.
 */
import { ProtocolError, SmoothAgentClient } from '../client.js';
import type { Citation } from '../types.js';
import type { ChatWidgetConfig } from './config.js';

export type { Citation };

export type Role = 'user' | 'assistant';

export interface ChatMessage {
    id: string;
    role: Role;
    /** Accumulated text (assistant messages grow as tokens stream in). */
    text: string;
    /** True while an assistant message is still streaming. */
    streaming: boolean;
    /**
     * Sources that grounded an assistant answer, when the terminal
     * `eventual_response` carried any. Optional + back-compatible: absent when
     * the turn used no knowledge sources (or for user messages). Read
     * defensively off the terminal event — see {@link extractCitations}.
     */
    citations?: Citation[];
}

export type ConnectionStatus = 'idle' | 'connecting' | 'ready' | 'error' | 'closed';

export interface ConversationEvents {
    /** Fired whenever the message list changes (append, token delta, finalize). */
    onMessages: (messages: ChatMessage[]) => void;
    /** Fired on connection-status transitions. */
    onStatus: (status: ConnectionStatus, detail?: string) => void;
}

/** Pull the final assistant text out of an `eventual_response` data payload. */
function extractFinalText(response: unknown): string | null {
    if (!response || typeof response !== 'object') return null;
    const r = response as { responseParts?: unknown };
    if (Array.isArray(r.responseParts)) {
        return r.responseParts.filter((p): p is string => typeof p === 'string').join('\n\n');
    }
    return null;
}

/**
 * Pull the grounding {@link Citation}s out of a terminal `eventual_response`.
 *
 * The protocol client types these (`eventual_response.data.data.citations`),
 * but they're optional and back-compatible — absent when the turn used no
 * knowledge sources. We read them defensively (tolerating their total absence,
 * non-array shapes, and missing fields) so a server that doesn't emit them, or
 * an older client, can't break rendering. Each citation always carries
 * `id`/`title`/`snippet`/`score`; `url` is present only for web-sourced docs.
 */
function extractCitations(inner: unknown): Citation[] {
    if (!inner || typeof inner !== 'object') return [];
    const raw = (inner as { citations?: unknown }).citations;
    if (!Array.isArray(raw)) return [];
    const out: Citation[] = [];
    for (const c of raw) {
        if (!c || typeof c !== 'object') continue;
        const obj = c as Record<string, unknown>;
        const id = typeof obj.id === 'string' ? obj.id : '';
        const title = typeof obj.title === 'string' ? obj.title : id || 'Source';
        const snippet = typeof obj.snippet === 'string' ? obj.snippet : '';
        const url = typeof obj.url === 'string' && obj.url ? obj.url : undefined;
        const score = typeof obj.score === 'number' ? obj.score : 0;
        out.push({ id, title, snippet, score, url });
    }
    return out;
}

export class ConversationController {
    private readonly config: ChatWidgetConfig;
    private readonly events: ConversationEvents;
    private client: SmoothAgentClient | null = null;
    private sessionId: string | null = null;
    private readonly messages: ChatMessage[] = [];
    private status: ConnectionStatus = 'idle';
    private seq = 0;

    constructor(config: ChatWidgetConfig, events: ConversationEvents) {
        this.config = config;
        this.events = events;
    }

    get connectionStatus(): ConnectionStatus {
        return this.status;
    }

    private nextId(prefix: string): string {
        this.seq += 1;
        return `${prefix}-${this.seq}-${Date.now().toString(36)}`;
    }

    private setStatus(status: ConnectionStatus, detail?: string): void {
        this.status = status;
        this.events.onStatus(status, detail);
    }

    private emitMessages(): void {
        // Hand out a shallow copy so the view can't mutate internal state.
        this.events.onMessages(this.messages.map((m) => ({ ...m })));
    }

    /** Open the transport and create a conversation session. Idempotent. */
    async connect(): Promise<void> {
        if (this.status === 'connecting' || this.status === 'ready') return;
        this.setStatus('connecting');
        try {
            this.client = new SmoothAgentClient({ url: this.config.endpoint });
            await this.client.connect();
            const session = await this.client.createConversationSession({
                agentId: this.config.agentId,
                userName: this.config.userName,
                userEmail: this.config.userEmail,
            });
            this.sessionId = session.sessionId;
            this.setStatus('ready');
        } catch (err) {
            this.setStatus('error', err instanceof Error ? err.message : String(err));
            throw err;
        }
    }

    /**
     * Submit a user message. Appends the user bubble immediately, then streams the
     * assistant reply token-by-token, finalizing on `eventual_response`.
     */
    async send(text: string): Promise<void> {
        const trimmed = text.trim();
        if (!trimmed) return;
        if (!this.client || !this.sessionId || this.status !== 'ready') {
            await this.connect();
        }
        if (!this.client || !this.sessionId) {
            throw new Error('Conversation is not connected');
        }

        // 1. User bubble.
        this.messages.push({ id: this.nextId('u'), role: 'user', text: trimmed, streaming: false });

        // 2. Placeholder assistant bubble we grow as tokens arrive.
        const assistant: ChatMessage = { id: this.nextId('a'), role: 'assistant', text: '', streaming: true };
        this.messages.push(assistant);
        this.emitMessages();

        try {
            const turn = this.client.sendMessage({ sessionId: this.sessionId, message: trimmed, stream: true });

            for await (const event of turn) {
                if (event.type === 'stream_token') {
                    const token = event.token ?? event.data?.token ?? '';
                    if (token) {
                        assistant.text += token;
                        this.emitMessages();
                    }
                }
            }

            const final = await turn;
            const inner = final.data?.data;
            const finalText = extractFinalText(inner?.response);
            if (finalText && finalText.length > assistant.text.length) {
                assistant.text = finalText;
            }
            if (!assistant.text) {
                assistant.text = '(no response)';
            }
            // Attach grounding sources from the terminal event, when present.
            const citations = extractCitations(inner);
            if (citations.length > 0) {
                assistant.citations = citations;
            }
            assistant.streaming = false;
            this.emitMessages();
        } catch (err) {
            assistant.streaming = false;
            const message =
                err instanceof ProtocolError
                    ? `Error: ${err.message}`
                    : (this.config.connectionErrorMessage ?? "We couldn't reach the chat.");
            assistant.text = assistant.text ? `${assistant.text}\n\n${message}` : message;
            this.emitMessages();
            this.setStatus('error', err instanceof Error ? err.message : String(err));
        }
    }

    /** Tear down the underlying client. */
    disconnect(): void {
        this.client?.disconnect('widget closed');
        this.client = null;
        this.sessionId = null;
        this.setStatus('closed');
    }
}
