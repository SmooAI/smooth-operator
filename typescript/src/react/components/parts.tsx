/**
 * Presentational building blocks.
 *
 * Each part is a thin, styling-light view over the data `useConversation`
 * produces. They render semantic markup with `smooth-chat__*` class names whose
 * styling lives entirely in `styles.css` and is driven by `--smooth-*` CSS
 * variables — so retheming is CSS, never prop-drilling. Every part also forwards
 * `className` so you can layer Tailwind utilities on top, and you can ignore
 * these entirely and render the hook's state yourself.
 */
import { useState, type FormEvent, type KeyboardEvent } from 'react';
import { safeHttpUrl } from '../response.js';
import type { ChatMessage, Citation, ConnectionStatus } from '../types.js';

function cx(...parts: (string | false | undefined)[]): string {
    return parts.filter(Boolean).join(' ');
}

/** A list of grounding sources rendered under an assistant bubble. */
export function Citations({ citations, className }: { citations: Citation[]; className?: string }) {
    if (!citations.length) return null;
    return (
        <div className={cx('smooth-chat__sources', className)}>
            <details>
                <summary>
                    {citations.length} source{citations.length === 1 ? '' : 's'}
                </summary>
                <ol>
                    {citations.map((c) => {
                        const href = safeHttpUrl(c.url);
                        return (
                            <li key={c.id || c.title}>
                                {href ? (
                                    <a className="smooth-chat__source-title" href={href} target="_blank" rel="noreferrer noopener">
                                        {c.title}
                                    </a>
                                ) : (
                                    <span className="smooth-chat__source-title">{c.title}</span>
                                )}
                                {c.snippet ? <span className="smooth-chat__source-snippet">{c.snippet}</span> : null}
                            </li>
                        );
                    })}
                </ol>
            </details>
        </div>
    );
}

/** A single message bubble (+ its citations + streaming cursor). */
export function MessageBubble({ message, className }: { message: ChatMessage; className?: string }) {
    return (
        <>
            <div className={cx('smooth-chat__bubble', `smooth-chat__bubble--${message.role}`, message.streaming && 'smooth-chat__bubble--streaming', className)}>
                {message.text}
                {message.streaming ? <span className="smooth-chat__cursor" aria-hidden="true" /> : null}
            </div>
            {message.role === 'assistant' && message.citations?.length ? <Citations citations={message.citations} /> : null}
        </>
    );
}

/** The scrollable message column. Renders an optional greeting before any messages. */
export function MessageList({ messages, greeting, className }: { messages: ChatMessage[]; greeting?: string; className?: string }) {
    return (
        <div className={cx('smooth-chat__messages', className)} role="log" aria-live="polite">
            {messages.length === 0 && greeting ? <div className="smooth-chat__bubble smooth-chat__bubble--assistant smooth-chat__greeting">{greeting}</div> : null}
            {messages.map((m) => (
                <MessageBubble key={m.id} message={m} />
            ))}
        </div>
    );
}

/** The input row. Calls `onSend` on submit / Enter (Shift+Enter inserts a newline). */
export function Composer({
    onSend,
    disabled,
    placeholder = 'Type a message…',
    className,
}: {
    onSend: (text: string) => void;
    disabled?: boolean;
    placeholder?: string;
    className?: string;
}) {
    const [value, setValue] = useState('');

    const submit = () => {
        const text = value.trim();
        if (!text || disabled) return;
        setValue('');
        onSend(text);
    };

    const onSubmit = (e: FormEvent) => {
        e.preventDefault();
        submit();
    };

    const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
        if (e.key === 'Enter' && !e.shiftKey) {
            e.preventDefault();
            submit();
        }
    };

    return (
        <form className={cx('smooth-chat__composer', className)} onSubmit={onSubmit}>
            <textarea
                className="smooth-chat__input"
                value={value}
                placeholder={placeholder}
                onChange={(e) => setValue(e.target.value)}
                onKeyDown={onKeyDown}
                rows={1}
                aria-label="Message"
            />
            <button className="smooth-chat__send" type="submit" disabled={disabled || value.trim().length === 0}>
                Send
            </button>
        </form>
    );
}

const STATUS_LABEL: Record<ConnectionStatus, string> = {
    idle: 'Idle',
    connecting: 'Connecting…',
    ready: 'Online',
    error: 'Connection error',
    closed: 'Disconnected',
};

/** A small connection-status label. */
export function ConnectionStatusLabel({ status, className }: { status: ConnectionStatus; className?: string }) {
    return (
        <span className={cx('smooth-chat__status', `smooth-chat__status--${status}`, className)} aria-live="polite">
            {STATUS_LABEL[status]}
        </span>
    );
}
