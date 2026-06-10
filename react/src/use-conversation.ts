/**
 * useConversation — the headless core of the React bindings.
 *
 * This hook owns the entire conversation lifecycle (connect → session → stream)
 * and exposes nothing but state + actions, so you can render a completely custom
 * UI with zero styling opinions from this package. `<SmoothChat>` and the parts
 * components are thin views over exactly this hook.
 *
 * It is the React analogue of the widget's `ConversationController`: same wire
 * flow, same defensive payload reading, but message state lives in React so your
 * components re-render on every streamed token.
 */
import { ProtocolError, SmoothAgentClient } from '@smooai/smooth-operator';
import { useCallback, useEffect, useRef, useState } from 'react';
import { extractCitations, extractFinalText } from './response.js';
import type { ChatMessage, ConnectionStatus } from './types.js';

export interface UseConversationOptions {
    /** WebSocket endpoint, e.g. `wss://your-host/ws`. Ignored if `client` is given. */
    url?: string;
    /**
     * Bring your own pre-constructed {@link SmoothAgentClient} (e.g. one created by
     * a {@link SmoothOperatorProvider} and shared, or one with a custom transport
     * for tests). When provided, the hook does NOT own the client's lifecycle —
     * it won't disconnect it on unmount.
     */
    client?: SmoothAgentClient;
    /** UUID of the agent to converse with. */
    agentId: string;
    /** Optional display name for the user participant. */
    userName?: string;
    /** Optional email for the user participant. */
    userEmail?: string;
    /**
     * Short-lived auth token for BYO-auth deployments. Appended to the WS URL as
     * `?token=…` (browsers can't set WebSocket headers), which the server reads
     * into the request `Principal` / `AccessContext`. Ignored if `client` is given.
     */
    authToken?: string;
    /** Connect automatically on mount. Default `true`. */
    autoConnect?: boolean;
    /** Message shown in the assistant bubble when the connection fails mid-turn. */
    connectionErrorMessage?: string;
}

export interface UseConversationResult {
    /** Current connection lifecycle state. */
    status: ConnectionStatus;
    /** The ordered message list (re-renders on every streamed token). */
    messages: ChatMessage[];
    /** Last error message, or `null`. */
    error: string | null;
    /** The active session id once a session has been created. */
    sessionId: string | null;
    /** Open the transport + create a conversation session. Idempotent. */
    connect: () => Promise<void>;
    /** Submit a user message and stream the assistant reply into `messages`. */
    send: (text: string) => Promise<void>;
    /** Tear down the client (only if this hook owns it) and reset to `closed`. */
    disconnect: () => void;
}

/** Append `?token=` / `&token=` to a WS URL for BYO-auth. */
function withToken(url: string, token: string | undefined): string {
    if (!token) return url;
    const sep = url.includes('?') ? '&' : '?';
    return `${url}${sep}token=${encodeURIComponent(token)}`;
}

export function useConversation(options: UseConversationOptions): UseConversationResult {
    const { url, client: externalClient, agentId, userName, userEmail, authToken, autoConnect = true, connectionErrorMessage } = options;

    const [status, setStatus] = useState<ConnectionStatus>('idle');
    const [messages, setMessages] = useState<ChatMessage[]>([]);
    const [error, setError] = useState<string | null>(null);

    // Mutable mirrors so streaming token deltas don't fight React's batching: we
    // mutate in place, then publish an immutable snapshot via setMessages.
    const messagesRef = useRef<ChatMessage[]>([]);
    const clientRef = useRef<SmoothAgentClient | null>(null);
    const ownsClientRef = useRef(false);
    const sessionIdRef = useRef<string | null>(null);
    const seqRef = useRef(0);
    const statusRef = useRef<ConnectionStatus>('idle');

    const setStatusBoth = useCallback((next: ConnectionStatus, detail?: string) => {
        statusRef.current = next;
        setStatus(next);
        if (next === 'error' && detail) setError(detail);
        if (next === 'ready' || next === 'connecting') setError(null);
    }, []);

    const nextId = useCallback((prefix: string) => {
        seqRef.current += 1;
        return `${prefix}-${seqRef.current}-${Date.now().toString(36)}`;
    }, []);

    const publish = useCallback(() => {
        setMessages(messagesRef.current.map((m) => ({ ...m })));
    }, []);

    const connect = useCallback(async () => {
        if (statusRef.current === 'connecting' || statusRef.current === 'ready') return;
        setStatusBoth('connecting');
        try {
            let client = externalClient ?? clientRef.current;
            if (!client) {
                if (!url) throw new Error('useConversation requires either `url` or `client`');
                client = new SmoothAgentClient({ url: withToken(url, authToken) });
                ownsClientRef.current = true;
                await client.connect();
            } else if (client === externalClient) {
                ownsClientRef.current = false;
            }
            clientRef.current = client;
            const session = await client.createConversationSession({ agentId, userName, userEmail });
            sessionIdRef.current = session.sessionId;
            setStatusBoth('ready');
        } catch (err) {
            setStatusBoth('error', err instanceof Error ? err.message : String(err));
            throw err;
        }
    }, [externalClient, url, authToken, agentId, userName, userEmail, setStatusBoth]);

    const send = useCallback(
        async (text: string) => {
            const trimmed = text.trim();
            if (!trimmed) return;
            if (!clientRef.current || !sessionIdRef.current || statusRef.current !== 'ready') {
                await connect();
            }
            const client = clientRef.current;
            const sessionId = sessionIdRef.current;
            if (!client || !sessionId) throw new Error('Conversation is not connected');

            messagesRef.current.push({ id: nextId('u'), role: 'user', text: trimmed, streaming: false });
            const assistant: ChatMessage = { id: nextId('a'), role: 'assistant', text: '', streaming: true };
            messagesRef.current.push(assistant);
            publish();

            try {
                const turn = client.sendMessage({ sessionId, message: trimmed, stream: true });
                for await (const event of turn) {
                    if (event.type === 'stream_token') {
                        const token = event.token ?? event.data?.token ?? '';
                        if (token) {
                            assistant.text += token;
                            publish();
                        }
                    }
                }
                const final = await turn;
                const inner = final.data?.data;
                const finalText = extractFinalText(inner?.response);
                if (finalText && finalText.length > assistant.text.length) assistant.text = finalText;
                if (!assistant.text) assistant.text = '(no response)';
                const citations = extractCitations(inner);
                if (citations.length > 0) assistant.citations = citations;
                assistant.streaming = false;
                publish();
            } catch (err) {
                assistant.streaming = false;
                const message =
                    err instanceof ProtocolError
                        ? `Error: ${err.message}`
                        : (connectionErrorMessage ?? "We couldn't reach the chat. Please try again in a moment.");
                assistant.text = assistant.text ? `${assistant.text}\n\n${message}` : message;
                publish();
                setStatusBoth('error', err instanceof Error ? err.message : String(err));
            }
        },
        [connect, nextId, publish, connectionErrorMessage, setStatusBoth],
    );

    const disconnect = useCallback(() => {
        if (ownsClientRef.current) clientRef.current?.disconnect('conversation closed');
        clientRef.current = externalClient ?? null;
        sessionIdRef.current = null;
        setStatusBoth('closed');
    }, [externalClient, setStatusBoth]);

    // Auto-connect once on mount; tear down only the client we own.
    useEffect(() => {
        let cancelled = false;
        if (autoConnect) {
            connect().catch(() => {
                /* surfaced via status/error */
            });
        }
        return () => {
            cancelled = true;
            void cancelled;
            if (ownsClientRef.current) {
                clientRef.current?.disconnect('component unmounted');
                clientRef.current = null;
                ownsClientRef.current = false;
            }
        };
        // Connect identity changes when its inputs change; re-running re-establishes the session.
    }, [autoConnect, connect]);

    return { status, messages, error, sessionId: sessionIdRef.current, connect, send, disconnect };
}
