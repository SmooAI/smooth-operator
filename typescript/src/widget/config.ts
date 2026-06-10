/**
 * Public configuration surface for the chat widget.
 *
 * A host page configures the widget either declaratively (HTML attributes on the
 * `<smooth-agent-chat>` element) or programmatically (passing this object to
 * {@link mountChatWidget} / `element.configure(...)`).
 */
export interface ChatWidgetTheme {
    /** Foreground text color for the widget chrome. */
    text?: string;
    /** Panel background color. */
    background?: string;
    /** Primary accent (launcher button, send button, outbound bubble). */
    primary?: string;
    /** Text color rendered on top of `primary`. */
    primaryText?: string;
    /** Inbound (assistant) chat bubble background. */
    assistantBubble?: string;
    /** Inbound (assistant) chat bubble text color. */
    assistantBubbleText?: string;
    /** Outbound (user) chat bubble background. Defaults to `primary`. */
    userBubble?: string;
    /** Outbound (user) chat bubble text color. Defaults to `primaryText`. */
    userBubbleText?: string;
    /** Border color for the panel and input. */
    border?: string;
}

/**
 * Layout mode for the widget.
 *
 * - `"popover"` (default) — the embeddable launcher bubble + floating panel.
 * - `"fullpage"` — no launcher; the chat fills its container/viewport with a
 *   branded header, a scrollable message list, and an input bar. Ideal for a
 *   dedicated support page (`/chat`, a docs site sidebar, an iframe, …).
 */
export type ChatWidgetMode = 'popover' | 'fullpage';

export interface ChatWidgetConfig {
    /**
     * smooth-operator WebSocket endpoint, e.g.
     * `ws://localhost:8787/ws` (local dev) or your deployed `wss://…/ws` URL.
     */
    endpoint: string;
    /**
     * Layout mode — `"popover"` (default, launcher + floating panel) or
     * `"fullpage"` (chat fills its container; no launcher). See {@link ChatWidgetMode}.
     */
    mode?: ChatWidgetMode;
    /** UUID of the agent to start a conversation session with. */
    agentId: string;
    /** Display name for the agent (header label). Defaults to "Assistant". */
    agentName?: string;
    /** Optional display name for the user participant. */
    userName?: string;
    /** Optional email address for the user participant. */
    userEmail?: string;
    /** Placeholder text for the message input. */
    placeholder?: string;
    /** Greeting rendered when the conversation opens (before any messages). */
    greeting?: string;
    /** Message shown when the connection cannot be (re)established. */
    connectionErrorMessage?: string;
    /** Start the panel open instead of collapsed to the launcher. */
    startOpen?: boolean;
    /** Theme overrides. */
    theme?: ChatWidgetTheme;
}

/** Resolve a partial config against the built-in defaults. */
export function resolveConfig(config: ChatWidgetConfig): Required<Omit<ChatWidgetConfig, 'theme' | 'userName' | 'userEmail'>> & {
    theme: Required<ChatWidgetTheme>;
    userName?: string;
    userEmail?: string;
} {
    const theme = config.theme ?? {};
    const primary = theme.primary ?? '#00a6a6';
    const primaryText = theme.primaryText ?? '#f8fafc';
    return {
        endpoint: config.endpoint,
        mode: config.mode ?? 'popover',
        agentId: config.agentId,
        agentName: config.agentName ?? 'Assistant',
        userName: config.userName,
        userEmail: config.userEmail,
        placeholder: config.placeholder ?? 'Type a message…',
        greeting: config.greeting ?? 'Hi! How can I help you today?',
        connectionErrorMessage: config.connectionErrorMessage ?? "We couldn't reach the chat. Please try again in a moment.",
        startOpen: config.startOpen ?? false,
        theme: {
            text: theme.text ?? '#f8fafc',
            background: theme.background ?? '#040d30',
            primary,
            primaryText,
            assistantBubble: theme.assistantBubble ?? '#06134b',
            assistantBubbleText: theme.assistantBubbleText ?? '#f8fafc',
            userBubble: theme.userBubble ?? primary,
            userBubbleText: theme.userBubbleText ?? primaryText,
            border: theme.border ?? '#0a1f7a',
        },
    };
}
