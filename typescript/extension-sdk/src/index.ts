/**
 * @smooai/smooth-extension-sdk — build SEP (Smooth Extension Protocol)
 * extensions in TypeScript.
 *
 * An extension is a subprocess speaking JSON-RPC 2.0 ndjson over stdio to any
 * SEP host (smooth-operator-core and its polyglot servers). Describe it with
 * `defineExtension`/`defineTool`, `serve()` it, test it in-process with
 * `createTestHost`, and gate it against the shared fixtures with
 * `runConformance`.
 */
export { defineExtension, defineTool, Extension } from './extension.js';
export type { ExtensionSetup, SmoothApi, ToolDef, ToolContext, ToolReturn, EventHandler, HookResult, ConnectHandle } from './extension.js';
export { createTestHost } from './test-host.js';
export type { TestHost, CallToolOptions } from './test-host.js';
export { runConformance, DEFAULT_SPEC_DIR } from './conformance.js';
export type { ConformanceReport, ConformanceStep, RunConformanceOptions } from './conformance.js';
export { toJsonSchema } from './schema.js';
export type { ParameterSchema } from './schema.js';
export { Peer, RpcError } from './jsonrpc.js';
export type { JsonRpcFrame } from './jsonrpc.js';
export { stdioTransport, linkedPair } from './transport.js';
export type { Transport } from './transport.js';
export { PROTOCOL_VERSION, method, errorCode } from './protocol.js';
export type {
    Context,
    InitializeParams,
    InitializeResult,
    Registrations,
    ToolRegistration,
    ToolExecuteParams,
    ToolExecuteResult,
    ToolUpdateParams,
    EventParams,
    HookParams,
    HookOutcome,
} from './protocol.js';
