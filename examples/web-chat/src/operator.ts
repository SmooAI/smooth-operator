// operator.ts — a React hook that speaks the smooth-operator WebSocket protocol
// through the repo's own published SDK (`@smooai/smooth-operator`). It is a
// trimmed port of the daemon PWA's `useOperator`: the SDK client owns the
// connection + request correlation + streaming-turn lifecycle, and this hook
// layers on the presentation model the daemon proved out — an interleaved
// text/tool block list, a conversation sidebar, and oldest-first history.
//
// Everything here drives a *real* running server; there is no mock. See the
// README for how to start `smooth-operator-server` and point this at it.

import { SmoothAgentClient, type ConversationSummary } from '@smooai/smooth-operator';
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';

/** The agent's live presence — what the header reflects. */
export type AgentState = 'connecting' | 'offline' | 'awake' | 'thinking' | 'speaking' | 'awaiting';

export interface ToolCall {
    id: string;
    name: string;
    args: string;
    result?: string;
    isError?: boolean;
    done: boolean;
}

/** One ordered segment of an assistant turn: a run of prose, or a tool call.
 * `blocks` preserves the interleave order the model produced (say a bit → call a
 * tool → say a bit → …) so the UI shows tools INLINE between text instead of
 * piling every chip at the top and concatenating all prose at the bottom. */
export type MessageBlock = { kind: 'text'; text: string } | { kind: 'tool'; tool: ToolCall };

export interface ChatMessage {
    id: string;
    role: 'user' | 'assistant' | 'system';
    content: string;
    /** Reasoning-model "thinking" — shown collapsed, never folded into the answer. */
    reasoning: string;
    /** Text + tool segments in the order the model streamed them (assistant turns). */
    blocks: MessageBlock[];
    streaming: boolean;
}

/** A parked write-tool the agent needs a human verdict on. `turnRequestId` is the
 * requestId of the streaming turn it belongs to — the SDK resumes that exact turn
 * when we confirm. */
export interface Approval {
    turnRequestId: string;
    tool: string;
    description: string;
}

export interface Status {
    connected: boolean;
    error?: string;
}

interface OperatorApi {
    state: AgentState;
    messages: ChatMessage[];
    approvals: Approval[];
    status: Status;
    sendMessage: (text: string) => void;
    respond: (turnRequestId: string, approved: boolean) => void;
    conversations: ConversationSummary[];
    activeConversationId: string | null;
    resumeConversation: (conversationId: string) => void;
    newConversation: () => void;
}

/** Resolve the server WS URL + optional token. Vite env vars win; a `?url=` /
 * `?token=` query param is a convenient override for quick manual testing. */
function resolveTarget(): { url: string; token: string; agentId: string } {
    const params = new URLSearchParams(window.location.search);
    const env = import.meta.env;
    const url = params.get('url') ?? env.VITE_SMOOTH_WS_URL ?? 'ws://localhost:8787/ws';
    const token = params.get('token') ?? env.VITE_SMOOTH_TOKEN ?? '';
    // The reference/no-auth server accepts any agent id; a real deployment passes
    // the agent's UUID. Keep it stable across a page session so resumes line up.
    const agentId = env.VITE_SMOOTH_AGENT_ID ?? crypto.randomUUID();
    return { url, token, agentId };
}

let msgSeq = 0;
const nextId = (p: string) => `${p}-${++msgSeq}`;

/** Flatten a history message's content to text (server shape = `content.text`;
 * tolerate the older `content.items[]` shape too). */
function historyText(m: any): string {
    const c = m?.content;
    if (c && typeof c === 'object') {
        if (typeof c.text === 'string') return c.text;
        if (Array.isArray(c.items)) return c.items.map((i: any) => i?.text ?? '').join('');
    }
    if (typeof c === 'string') return c;
    return m?.text ?? '';
}

/** Render server history into our ChatMessage model. The server returns messages
 * newest-first, so sort ascending by `createdAt` to read chronologically. */
function renderHistory(raw: any[]): ChatMessage[] {
    const chronological = (raw ?? []).slice().sort((a, b) => (Date.parse(a?.createdAt ?? '') || 0) - (Date.parse(b?.createdAt ?? '') || 0));
    return chronological.map((m) => {
        const content = historyText(m);
        const isUser = m?.direction === 'inbound' || m?.role === 'user';
        const role: ChatMessage['role'] = isUser ? 'user' : 'assistant';
        return {
            id: nextId('h'),
            role,
            content,
            reasoning: '',
            blocks: role === 'assistant' ? [{ kind: 'text', text: content }] : [],
            streaming: false,
        };
    });
}

export function useOperator(): OperatorApi {
    const [messages, setMessages] = useState<ChatMessage[]>([]);
    const [approvals, setApprovals] = useState<Approval[]>([]);
    const [conversations, setConversations] = useState<ConversationSummary[]>([]);
    const [activeConversationId, setActiveConversationId] = useState<string | null>(null);
    const [connected, setConnected] = useState(false);
    const [turnActive, setTurnActive] = useState(false);
    const [streaming, setStreaming] = useState(false);
    const [error, setError] = useState<string | undefined>();

    const clientRef = useRef<SmoothAgentClient | null>(null);
    const sessionRef = useRef<string | null>(null);
    const targetRef = useRef(resolveTarget());

    // Mutate the in-flight assistant message (the last streaming one).
    const patchStreaming = useCallback((fn: (m: ChatMessage) => ChatMessage) => {
        setMessages((prev) => {
            for (let i = prev.length - 1; i >= 0; i--) {
                if (prev[i].role === 'assistant' && prev[i].streaming) {
                    const copy = prev.slice();
                    copy[i] = fn(copy[i]);
                    return copy;
                }
            }
            return prev;
        });
    }, []);

    const refreshConversations = useCallback(async () => {
        const client = clientRef.current;
        if (!client) return;
        try {
            const { conversations } = await client.listConversations();
            setConversations(conversations);
        } catch {
            /* best-effort; the sidebar just stays as-is */
        }
    }, []);

    /** Consume a streaming turn, folding its events into the open assistant
     * message's block list — the interleave logic ported from the daemon. */
    const consumeTurn = useCallback(
        async (turn: import('@smooai/smooth-operator').MessageTurn) => {
            try {
                for await (const ev of turn) {
                    const v = ev as any;
                    switch (v.type) {
                        case 'stream_token': {
                            setStreaming(true);
                            const tok = v.token ?? v.data?.token ?? '';
                            patchStreaming((m) => {
                                const blocks = m.blocks.slice();
                                const last = blocks[blocks.length - 1];
                                // Grow the trailing text block, or open a new one if the
                                // last block was a tool — that interleaves prose with chips.
                                if (last && last.kind === 'text') blocks[blocks.length - 1] = { kind: 'text', text: last.text + tok };
                                else blocks.push({ kind: 'text', text: tok });
                                return { ...m, content: m.content + tok, blocks };
                            });
                            break;
                        }
                        case 'stream_reasoning':
                            patchStreaming((m) => ({ ...m, reasoning: m.reasoning + (v.token ?? v.data?.token ?? '') }));
                            break;
                        case 'stream_chunk': {
                            const st = v.data?.state;
                            const call = st?.rawResponse?.toolCall;
                            const res = st?.rawResponse?.toolResult;
                            if (call) {
                                const args = typeof call.arguments === 'string' ? call.arguments : JSON.stringify(call.arguments ?? {});
                                const tool: ToolCall = { id: nextId('t'), name: call.name ?? '', args, done: false };
                                patchStreaming((m) => ({ ...m, blocks: [...m.blocks, { kind: 'tool', tool }] }));
                            } else if (res) {
                                patchStreaming((m) => {
                                    const blocks = m.blocks.slice();
                                    for (let i = blocks.length - 1; i >= 0; i--) {
                                        const b = blocks[i];
                                        if (b.kind === 'tool' && b.tool.name === res.name && !b.tool.done) {
                                            blocks[i] = {
                                                kind: 'tool',
                                                tool: {
                                                    ...b.tool,
                                                    done: true,
                                                    isError: !!res.isError,
                                                    result: typeof res.result === 'string' ? res.result : JSON.stringify(res.result ?? ''),
                                                },
                                            };
                                            break;
                                        }
                                    }
                                    return { ...m, blocks };
                                });
                            }
                            break;
                        }
                        case 'write_confirmation_required': {
                            const d = v.data?.data ?? v.data ?? {};
                            setApprovals((prev) => [
                                ...prev.filter((a) => a.turnRequestId !== turn.requestId),
                                { turnRequestId: turn.requestId, tool: d.toolId ?? 'tool', description: d.actionDescription ?? '' },
                            ]);
                            break;
                        }
                        default:
                            break;
                    }
                }
            } catch (err) {
                setMessages((prev) => [
                    ...prev,
                    { id: nextId('e'), role: 'system', content: err instanceof Error ? err.message : String(err), reasoning: '', blocks: [], streaming: false },
                ]);
            } finally {
                setTurnActive(false);
                setStreaming(false);
                setApprovals((prev) => prev.filter((a) => a.turnRequestId !== turn.requestId));
                patchStreaming((m) => ({ ...m, streaming: false }));
                void refreshConversations();
            }
        },
        [patchStreaming, refreshConversations],
    );

    // Open one connection + session for the app's lifetime.
    useEffect(() => {
        const { url, token, agentId } = targetRef.current;
        const client = new SmoothAgentClient({ url, token });
        clientRef.current = client;
        let cancelled = false;

        (async () => {
            try {
                await client.connect();
                const session = await client.createConversationSession({ agentId, userName: 'web-chat-example' });
                if (cancelled) return;
                sessionRef.current = session.sessionId;
                setActiveConversationId(session.conversationId);
                setConnected(true);
                setError(undefined);
                void refreshConversations();
            } catch (err) {
                if (!cancelled) setError(err instanceof Error ? err.message : String(err));
            }
        })();

        return () => {
            cancelled = true;
            client.disconnect('component unmounted');
            clientRef.current = null;
        };
    }, [refreshConversations]);

    const sendMessage = useCallback(
        (text: string) => {
            const body = text.trim();
            const client = clientRef.current;
            if (!body || !client || !sessionRef.current) return;
            setMessages((prev) => [
                ...prev,
                { id: nextId('u'), role: 'user', content: body, reasoning: '', blocks: [], streaming: false },
                { id: nextId('a'), role: 'assistant', content: '', reasoning: '', blocks: [], streaming: true },
            ]);
            setTurnActive(true);
            const turn = client.sendMessage({ sessionId: sessionRef.current, message: body, stream: true });
            void consumeTurn(turn);
        },
        [consumeTurn],
    );

    const respond = useCallback((turnRequestId: string, approved: boolean) => {
        const client = clientRef.current;
        if (!client || !sessionRef.current) return;
        setApprovals((prev) => prev.filter((a) => a.turnRequestId !== turnRequestId));
        client.confirmToolAction({ sessionId: sessionRef.current, requestId: turnRequestId, approved });
    }, []);

    const resumeConversation = useCallback(async (conversationId: string) => {
        const client = clientRef.current;
        if (!client || !conversationId) return;
        setActiveConversationId(conversationId);
        setMessages([]);
        setApprovals([]);
        sessionRef.current = null;
        const { agentId } = targetRef.current;
        const session = await client.createConversationSession({ agentId, conversationId, userName: 'web-chat-example' });
        sessionRef.current = session.sessionId;
        const { messages } = await client.getMessages({ sessionId: session.sessionId });
        setMessages(renderHistory(messages));
    }, []);

    const newConversation = useCallback(async () => {
        const client = clientRef.current;
        if (!client) return;
        setMessages([]);
        setApprovals([]);
        sessionRef.current = null;
        const { agentId } = targetRef.current;
        const session = await client.createConversationSession({ agentId, userName: 'web-chat-example' });
        sessionRef.current = session.sessionId;
        setActiveConversationId(session.conversationId);
        void refreshConversations();
    }, [refreshConversations]);

    const state: AgentState = useMemo(() => {
        if (error && !connected) return 'offline';
        if (!connected) return 'connecting';
        if (approvals.length) return 'awaiting';
        if (streaming) return 'speaking';
        if (turnActive) return 'thinking';
        return 'awake';
    }, [error, connected, approvals.length, streaming, turnActive]);

    return {
        state,
        messages,
        approvals,
        status: { connected, error },
        sendMessage,
        respond,
        conversations,
        activeConversationId,
        resumeConversation,
        newConversation,
    };
}
