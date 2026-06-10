/**
 * <SmoothChat> — the opinionated, drop-in chat surface.
 *
 * It wires `useConversation` to the presentational parts and applies the
 * `.smooth-chat` root class whose styling is 100% CSS-variable-driven. Theme it
 * any of three ways (see `theme.ts`): override `--smooth-*` in your CSS/Tailwind,
 * pass a `theme` prop (sets the variables inline), or restyle the
 * `smooth-chat__*` classes outright.
 *
 * Need full control? Skip this component and build your own view over
 * `useConversation` + the exported parts. This is just the batteries-included path.
 *
 * Remember to import the stylesheet once in your app:
 *   import '@smooai/smooth-operator-react/styles.css';
 */
import { themeToStyle, type ChatTheme } from '../theme.js';
import { useConversation, type UseConversationOptions } from '../use-conversation.js';
import { Composer, ConnectionStatusLabel, MessageList } from './parts.js';

export interface SmoothChatProps extends UseConversationOptions {
    /** Header label for the agent. Default "Assistant". */
    agentName?: string;
    /** Greeting rendered before any messages. */
    greeting?: string;
    /** Placeholder for the input. */
    placeholder?: string;
    /** Per-instance theme overrides (sets `--smooth-*` inline; wins over stylesheet rules). */
    theme?: ChatTheme;
    /** Show the small connection-status label in the header. Default `true`. */
    showStatus?: boolean;
    /** Extra class names on the `.smooth-chat` root (e.g. Tailwind layout utilities). */
    className?: string;
    /** Layout: `panel` (bordered card) or `fullpage` (fills its container). Default `panel`. */
    layout?: 'panel' | 'fullpage';
}

export function SmoothChat(props: SmoothChatProps) {
    const { agentName = 'Assistant', greeting, placeholder, theme, showStatus = true, className, layout = 'panel', ...conversationOptions } = props;

    const { status, messages, send } = useConversation(conversationOptions);
    const busy = messages.some((m) => m.streaming);

    return (
        <div
            className={['smooth-chat', `smooth-chat--${layout}`, className].filter(Boolean).join(' ')}
            style={themeToStyle(theme)}
            data-status={status}
        >
            <header className="smooth-chat__header">
                <span className="smooth-chat__title">{agentName}</span>
                {showStatus ? <ConnectionStatusLabel status={status} /> : null}
            </header>
            <MessageList messages={messages} greeting={greeting} />
            <Composer onSend={(text) => void send(text)} disabled={status === 'connecting' || busy} placeholder={placeholder} />
        </div>
    );
}
