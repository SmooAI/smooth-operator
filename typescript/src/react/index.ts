/**
 * @smooai/smooth-operator/react — React bindings for the smooth-operator
 * WebSocket protocol.
 *
 * Two layers, use whichever you need:
 *
 *   • Headless — `useConversation` (+ `SmoothOperatorProvider`) own the whole
 *     connect → session → stream lifecycle and expose only state + actions. Build
 *     any UI you like on top; this package imposes zero styling.
 *
 *   • Batteries-included — `<SmoothChat>` and the `parts` components render a
 *     complete, accessible chat surface themed entirely by `--smooth-*` CSS
 *     variables. Import the stylesheet once:
 *       import '@smooai/smooth-operator/react/styles.css';
 *
 * The protocol client itself lives in `@smooai/smooth-operator`; re-exported here
 * for convenience so you can construct/share a client without a second import.
 */
export { SmoothAgentClient, MessageTurn, ProtocolError, TurnTimeoutError, type SmoothAgentClientOptions } from '../index.js';

export { useConversation, type UseConversationOptions, type UseConversationResult } from './use-conversation.js';
export { SmoothOperatorProvider, useSmoothOperator, type SmoothOperatorProviderProps, type SmoothOperatorContextValue } from './provider.js';

export { themeToStyle, type ChatTheme } from './theme.js';
export { safeHttpUrl, extractCitations, extractFinalText } from './response.js';

export { SmoothChat, type SmoothChatProps } from './components/SmoothChat.js';
export { MessageList, MessageBubble, Citations, Composer, ConnectionStatusLabel } from './components/parts.js';

export type { ChatMessage, ConnectionStatus, Role, Citation } from './types.js';
