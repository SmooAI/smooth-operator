//! # smooth-operator-server
//!
//! The reference WebSocket service for smooth-operator. It speaks the
//! schema-driven protocol in `smooth-operator/spec/` over a smooth-operator-backed
//! knowledge-chat runtime, so the generated TypeScript / Go / .NET / Python
//! clients can connect and drive real LLM turns unmodified.
//!
//! ## Pieces
//!
//! - [`config`] — env-driven [`ServerConfig`](config::ServerConfig) (gateway URL
//!   / key / model / limits). The gateway key is optional at startup; without it
//!   `send_message` returns a clean `error` so protocol conformance is testable
//!   with zero credentials.
//! - [`protocol`] — builders for the server→client event envelopes, matched
//!   field-for-field to `spec/events/*.json`.
//! - [`state`] — shared [`AppState`](state::AppState): storage adapter + session
//!   registry.
//! - [`handler`] — action dispatch (`ping`, `create_conversation_session`,
//!   `get_session`, `send_message`).
//! - [`runner`] — the streaming, memory-carrying turn runner over a
//!   smooth-operator [`Agent`](smooth_operator_core::Agent).
//! - [`server`] — the axum app + per-connection socket loop. [`server::bind`]
//!   and [`server::router`] let tests boot the service in-process.
//! - [`admin`] — the auth-gated admin HTTP API (Phase 12) mounted under
//!   `/admin`: whoami, chat history, indexing status, document sets. Consumed by
//!   the Next.js management console (increment 2). See `docs/ADMIN-API.md`.
//!
//! ## Env contract (reused by every language's E2E harness)
//!
//! See [`config`] for the full table. The load-bearing ones:
//! `SMOOTH_AGENT_PORT`, `SMOOAI_GATEWAY_URL`, `SMOOAI_GATEWAY_KEY`,
//! `SMOOTH_AGENT_MODEL`, `SMOOTH_AGENT_SEED_KB`.

pub mod admin;
pub mod config;
pub mod embedder;
pub mod handler;
pub mod protocol;
pub mod reranker;
pub mod runner;
pub mod server;
pub mod state;

pub use config::ServerConfig;
pub use embedder::{build_embedder, EmbedderConfig};
pub use reranker::{build_reranker, RerankMode, RerankerConfig};
pub use server::{
    bind, build_state, build_state_from_env, build_state_from_env_async, router, run, serve_state,
    serve_state_on,
};
pub use state::AppState;
