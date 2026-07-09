/**
 * @smooai/smooth-operator — TypeScript protocol types + native client for the
 * smooth-operator WebSocket protocol.
 *
 * The protocol contract is defined by the language-neutral JSON Schemas in
 * `spec/`. The generated types (`./generated/types.ts`) are committed so consumers
 * don't need the generator; `./types.ts` layers the ergonomic discriminated
 * unions and guards on top.
 */
export * from './types.js';
export {
    SmoothAgentClient,
    MessageTurn,
    ProtocolError,
    TurnTimeoutError,
    type SmoothAgentClientOptions,
    type ConversationSummary,
    type ListConversationsResponse,
} from './client.js';
export {
    WebSocketTransport,
    type Transport,
    type TransportState,
    type WebSocketLike,
    type WebSocketFactory,
} from './transport.js';

// NOTE: the Node-only `ProtocolValidator` (it pulls in `ajv` + `node:fs`) is
// intentionally NOT part of this default barrel — that keeps the main entry
// browser-clean so the widget / React bindings / any browser app can import the
// client without dragging `ajv` into their bundle. Import it from the dedicated
// subpath instead:
//   import { ProtocolValidator } from '@smooai/smooth-operator/validate';
