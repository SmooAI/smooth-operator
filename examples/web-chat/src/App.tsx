// App.tsx — the whole chat UI: a conversation sidebar, an interleaved
// text/tool-chip transcript, a human-in-the-loop approval bar, and a composer.
// A deliberately minimal, dependency-light port of the daemon PWA so the
// interesting part — driving the protocol — stays legible.

import { useEffect, useRef, useState } from 'react';
import { useOperator, type ChatMessage, type ToolCall } from './operator';

const STATE_LABEL: Record<string, string> = {
    connecting: 'Connecting…',
    offline: 'Offline',
    awake: 'Ready',
    thinking: 'Thinking…',
    speaking: 'Responding…',
    awaiting: 'Waiting on you',
};

const STATE_DOT: Record<string, string> = {
    connecting: 'bg-amber-400 animate-pulse',
    offline: 'bg-rose-500',
    awake: 'bg-emerald-500',
    thinking: 'bg-sky-400 animate-pulse',
    speaking: 'bg-teal-400 animate-pulse',
    awaiting: 'bg-amber-400 animate-pulse',
};

function relTime(iso: string): string {
    const t = Date.parse(iso);
    if (!Number.isFinite(t)) return '';
    const secs = Math.round((Date.now() - t) / 1000);
    if (secs < 60) return 'just now';
    if (secs < 3600) return `${Math.floor(secs / 60)}m ago`;
    if (secs < 86400) return `${Math.floor(secs / 3600)}h ago`;
    return `${Math.floor(secs / 86400)}d ago`;
}

export default function App() {
    const op = useOperator();
    const [input, setInput] = useState('');
    const scrollRef = useRef<HTMLDivElement>(null);

    useEffect(() => {
        scrollRef.current?.scrollTo({ top: scrollRef.current.scrollHeight });
    }, [op.messages]);

    const submit = () => {
        if (!input.trim()) return;
        op.sendMessage(input);
        setInput('');
    };

    return (
        <div className="flex h-dvh bg-slate-950 text-slate-100">
            {/* Sidebar */}
            <aside className="flex w-72 shrink-0 flex-col border-r border-slate-800 bg-slate-900/50">
                <div className="flex items-center gap-2 px-4 py-4">
                    <span className="text-lg font-bold">
                        <span className="text-orange-400">smoo</span>
                        <span className="text-teal-400">th</span>
                    </span>
                    <span className="text-xs text-slate-500">web-chat example</span>
                </div>
                <button
                    onClick={op.newConversation}
                    className="mx-3 mb-3 rounded-lg bg-teal-600 px-3 py-2 text-sm font-semibold text-white transition hover:bg-teal-500"
                >
                    + New chat
                </button>
                <nav className="min-h-0 flex-1 space-y-1 overflow-y-auto px-2 pb-4">
                    {op.conversations.length === 0 ? (
                        <p className="px-3 py-6 text-center text-xs text-slate-600">No conversations yet.</p>
                    ) : (
                        op.conversations.map((c) => {
                            const active = c.conversationId === op.activeConversationId;
                            return (
                                <button
                                    key={c.conversationId}
                                    onClick={() => op.resumeConversation(c.conversationId)}
                                    className={`flex w-full flex-col gap-0.5 rounded-lg px-3 py-2 text-left transition ${
                                        active ? 'bg-teal-500/10 ring-1 ring-teal-500/40' : 'hover:bg-slate-800/60'
                                    }`}
                                >
                                    <span className="truncate text-sm font-medium">{c.title || 'Untitled'}</span>
                                    <span className="text-xs text-slate-500">
                                        {relTime(c.updatedAt)}
                                        {c.messageCount ? ` · ${c.messageCount} msg` : ''}
                                    </span>
                                </button>
                            );
                        })
                    )}
                </nav>
            </aside>

            {/* Main */}
            <main className="flex min-w-0 flex-1 flex-col">
                <header className="flex items-center gap-2 border-b border-slate-800 px-6 py-3">
                    <span className={`size-2.5 rounded-full ${STATE_DOT[op.state] ?? 'bg-slate-500'}`} />
                    <span className="text-sm text-slate-300">{STATE_LABEL[op.state] ?? op.state}</span>
                    {op.status.error && <span className="ml-3 truncate text-xs text-rose-400">{op.status.error}</span>}
                </header>

                <div ref={scrollRef} className="flex-1 space-y-5 overflow-y-auto px-6 py-6">
                    {op.messages.length === 0 && (
                        <p className="mt-20 text-center text-sm text-slate-600">
                            Say hello to the operator. Try “What is your return policy?” against the seeded knowledge base.
                        </p>
                    )}
                    {op.messages.map((m) => (
                        <MessageRow key={m.id} m={m} />
                    ))}
                </div>

                {/* Approvals — human-in-the-loop write-tool confirmations */}
                {op.approvals.map((a) => (
                    <div key={a.turnRequestId} className="mx-6 mb-3 rounded-lg border border-amber-500/40 bg-amber-500/10 px-4 py-3">
                        <p className="text-sm text-amber-200">
                            Approve <span className="font-mono font-semibold">{a.tool}</span>? {a.description}
                        </p>
                        <div className="mt-2 flex gap-2">
                            <button onClick={() => op.respond(a.turnRequestId, true)} className="rounded bg-emerald-600 px-3 py-1 text-xs font-semibold hover:bg-emerald-500">
                                Approve
                            </button>
                            <button onClick={() => op.respond(a.turnRequestId, false)} className="rounded bg-slate-700 px-3 py-1 text-xs font-semibold hover:bg-slate-600">
                                Deny
                            </button>
                        </div>
                    </div>
                ))}

                {/* Composer */}
                <div className="border-t border-slate-800 px-6 py-4">
                    <div className="flex items-end gap-2">
                        <textarea
                            value={input}
                            onChange={(e) => setInput(e.target.value)}
                            onKeyDown={(e) => {
                                if (e.key === 'Enter' && !e.shiftKey) {
                                    e.preventDefault();
                                    submit();
                                }
                            }}
                            rows={1}
                            placeholder={op.status.connected ? 'Message the operator…' : 'Connecting…'}
                            disabled={!op.status.connected}
                            className="max-h-40 min-h-[2.5rem] flex-1 resize-none rounded-xl border border-slate-700 bg-slate-900 px-4 py-2.5 text-sm outline-none focus:border-teal-500 disabled:opacity-50"
                        />
                        <button
                            onClick={submit}
                            disabled={!op.status.connected || !input.trim()}
                            className="rounded-xl bg-teal-600 px-5 py-2.5 text-sm font-semibold text-white transition hover:bg-teal-500 disabled:opacity-40"
                        >
                            Send
                        </button>
                    </div>
                </div>
            </main>
        </div>
    );
}

function MessageRow({ m }: { m: ChatMessage }) {
    if (m.role === 'system') {
        return <div className="rounded-lg border border-rose-500/30 bg-rose-500/5 px-3 py-2 text-sm text-rose-300">{m.content}</div>;
    }
    if (m.role === 'user') {
        return (
            <div className="flex justify-end">
                <div className="max-w-[85%] rounded-2xl rounded-br-md bg-teal-600/20 px-4 py-2.5 text-[0.95rem] leading-relaxed whitespace-pre-wrap">{m.content}</div>
            </div>
        );
    }
    // Assistant: prose and tool chips interleaved in the order the model produced them.
    return (
        <div className="flex gap-3">
            <span className="mt-1.5 size-2 shrink-0 rounded-full bg-gradient-to-b from-teal-400 to-sky-500" />
            <div className="min-w-0 flex-1 space-y-2">
                {m.reasoning && (
                    <details className="mb-1">
                        <summary className="cursor-pointer text-xs text-slate-500 select-none">{m.streaming && !m.content ? 'thinking…' : 'thought for a moment'}</summary>
                        <div className="mt-1 border-l-2 border-teal-500/25 pl-3 text-xs whitespace-pre-wrap text-slate-400 italic">{m.reasoning}</div>
                    </details>
                )}
                {m.blocks.map((b, i) =>
                    b.kind === 'tool' ? (
                        <ToolChip key={b.tool.id} t={b.tool} />
                    ) : b.text.trim() ? (
                        <div key={`t-${i}`} className="text-[0.95rem] leading-relaxed whitespace-pre-wrap text-slate-100">
                            {b.text}
                        </div>
                    ) : null,
                )}
                {m.streaming && m.blocks.length === 0 && <span className="text-slate-500">▋</span>}
            </div>
        </div>
    );
}

function ToolChip({ t }: { t: ToolCall }) {
    const arg = t.args.length > 80 ? `${t.args.slice(0, 80)}…` : t.args;
    return (
        <div className="my-1 overflow-hidden rounded-lg border border-slate-700 bg-slate-900/60">
            <div className="flex items-center gap-2 px-3 py-1.5 text-xs">
                <span className="font-mono font-medium text-teal-300">{t.name}</span>
                <span className="truncate font-mono text-slate-500">{arg}</span>
                <span className="ml-auto shrink-0">
                    {!t.done ? <span className="text-slate-500">running…</span> : t.isError ? <span className="text-rose-400">✗ error</span> : <span className="text-emerald-400">✓</span>}
                </span>
            </div>
            {t.done && t.result && (
                <pre className="max-h-32 overflow-y-auto border-t border-slate-700/60 px-3 py-1.5 font-mono text-[0.72rem] leading-relaxed text-slate-400">{t.result.slice(0, 600)}</pre>
            )}
        </div>
    );
}
