/// <reference types="vite/client" />

interface ImportMetaEnv {
    /** WebSocket endpoint of the smooth-operator server. Default `ws://localhost:8787/ws`. */
    readonly VITE_SMOOTH_WS_URL?: string;
    /** Optional bearer token for a token-gated server (appended as `?token=`). */
    readonly VITE_SMOOTH_TOKEN?: string;
    /** Optional agent UUID; defaults to a random one per page load. */
    readonly VITE_SMOOTH_AGENT_ID?: string;
}

interface ImportMeta {
    readonly env: ImportMetaEnv;
}
