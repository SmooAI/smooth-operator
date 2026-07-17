//! The **local deployment flavor** — an embeddable, zero-config server.
//!
//! This is the third deployment target alongside `deploy/k8s` (Kubernetes +
//! Postgres + NATS) and `deploy/sst` (AWS serverless): a self-contained server
//! with **everything in-memory** and **auth off**, meant to run on a developer
//! laptop or to be **embedded in-process** by a host (e.g. the smooth daemon).
//!
//! It needs no external services — no Postgres, no Redis, no NATS, no AWS — and
//! no secrets. It is exactly the default-flavor server the binary already boots
//! when no env is set ([`ServerConfig::from_env`](crate::config::ServerConfig::from_env)
//! defaults to in-memory storage, in-memory backplane, loopback bind, and admin
//! disabled), factored here so a host can boot it from code in a few lines
//! rather than by shelling out and setting env vars.
//!
//! ## In-process embed
//!
//! ```no_run
//! # async fn demo() -> anyhow::Result<()> {
//! // Boot a fully in-memory server on the default loopback addr, in the
//! // background, and get a handle to its real bound address + a shutdown switch.
//! let server = smooth_operator_server::local::LocalServer::builder()
//!     .seed_kb(true) // optional: load the demo knowledge docs
//!     .spawn()
//!     .await?;
//!
//! println!("local operator on ws://{}/ws", server.addr());
//! // ... use it (connect a client, run turns) ...
//!
//! server.shutdown().await; // graceful stop + join
//! # Ok(())
//! # }
//! ```
//!
//! ## Run to completion
//!
//! ```no_run
//! # async fn demo() -> anyhow::Result<()> {
//! // Or just run it in the foreground until killed (what the binary's no-env
//! // path effectively does):
//! smooth_operator_server::local::serve_local("127.0.0.1:8787").await?;
//! # Ok(())
//! # }
//! ```
//!
//! ## What "local flavor" pins
//!
//! Independent of ambient env, the local flavor always uses:
//!
//! - **storage** = in-memory ([`InMemoryStorageAdapter`](smooth_operator_adapter_memory::InMemoryStorageAdapter)),
//! - **backplane** = in-memory (single-process; no Redis/NATS),
//! - **auth** = none ([`NoAuthVerifier`](smooth_operator::auth::NoAuthVerifier)) — `/admin` is open, `/ws` boots,
//! - **widget auth** = permissive ([`PermissiveWidgetAuth`](smooth_operator::widget_auth::PermissiveWidgetAuth)),
//! - **bind** = a caller-supplied addr (default `127.0.0.1:8787`).
//!
//! The LLM gateway is still read from `SMOOAI_GATEWAY_URL` / `SMOOAI_GATEWAY_KEY`
//! (so a key in the environment enables live turns); with no key, `send_message`
//! returns a clean protocol `error` exactly as the keyless test path does.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use smooth_operator::adapter::StorageAdapter;
use smooth_operator::auth::{AuthVerifier, NoAuthVerifier};
use smooth_operator::tool_provider::ToolProvider;
use smooth_operator_core::tool::ToolHook;

use crate::config::{ServerConfig, StorageBackend};
use crate::server::{build_state, router};
use crate::state::AppState;

/// The default address the local flavor binds when the caller gives none —
/// loopback on the canonical WebSocket port, matching
/// [`config::DEFAULT_BIND`](crate::config::DEFAULT_BIND) +
/// [`config::DEFAULT_PORT`](crate::config::DEFAULT_PORT).
pub const DEFAULT_LOCAL_ADDR: &str = "127.0.0.1:8787";

/// Builder for the [local deployment flavor](self): a fully in-memory,
/// auth-off, single-process server, embeddable in-process.
///
/// All knobs are optional — `LocalServer::builder().spawn().await` boots the
/// default flavor (in-memory everything, loopback `:8787`, no auth, no seed).
/// Construct with [`LocalServer::builder`].
#[derive(Clone)]
pub struct LocalServerBuilder {
    addr: SocketAddr,
    seed_kb: bool,
    config: Option<ServerConfig>,
    auth: Option<Arc<dyn AuthVerifier>>,
    tool_provider: Option<Arc<dyn ToolProvider>>,
    tool_hooks: Vec<Arc<dyn ToolHook>>,
    serve_widget: bool,
    widget_token: Option<String>,
    strict_auth: bool,
    storage: Option<Arc<dyn StorageAdapter>>,
    persona: Option<String>,
    spa_router: Option<axum::Router>,
    extra_routes: Option<axum::Router>,
}

impl std::fmt::Debug for LocalServerBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalServerBuilder")
            .field("addr", &self.addr)
            .field("seed_kb", &self.seed_kb)
            .field("config", &self.config)
            // Never print the verifier's secrets — just its mode label.
            .field("auth", &self.auth.as_ref().map(|a| a.mode()))
            .field("tool_provider", &self.tool_provider.is_some())
            .field("serve_widget", &self.serve_widget)
            .finish()
    }
}

impl Default for LocalServerBuilder {
    fn default() -> Self {
        Self {
            addr: DEFAULT_LOCAL_ADDR
                .parse()
                .expect("DEFAULT_LOCAL_ADDR is a valid SocketAddr"),
            seed_kb: false,
            config: None,
            auth: None,
            tool_provider: None,
            tool_hooks: Vec::new(),
            serve_widget: false,
            widget_token: None,
            strict_auth: false,
            storage: None,
            persona: None,
            spa_router: None,
            extra_routes: None,
        }
    }
}

impl LocalServerBuilder {
    /// Bind on the given address instead of the default `127.0.0.1:8787`.
    ///
    /// Use port `0` for an ephemeral port (read the real one back from
    /// [`LocalServer::addr`] after [`spawn`](Self::spawn)).
    #[must_use]
    pub fn addr(mut self, addr: SocketAddr) -> Self {
        self.addr = addr;
        self
    }

    /// Seed the knowledge base with the demo docs on boot (default `false`).
    /// Handy for an embedded demo / smoke test with grounded answers.
    #[must_use]
    pub fn seed_kb(mut self, seed: bool) -> Self {
        self.seed_kb = seed;
        self
    }

    /// Install a custom [`AuthVerifier`] for the local flavor.
    ///
    /// Without this, the local flavor runs auth-off ([`NoAuthVerifier`]) — fine
    /// for pure loopback. Pass a
    /// [`LocalTokenVerifier`](smooth_operator::auth::LocalTokenVerifier) to gate
    /// stray local processes with a shared secret (recommended when binding
    /// beyond loopback, e.g. over a tailnet).
    #[must_use]
    pub fn auth(mut self, auth: Arc<dyn AuthVerifier>) -> Self {
        self.auth = Some(auth);
        self
    }

    /// Install a host [`ToolProvider`] so the runner merges its per-turn tools
    /// into every turn alongside the built-ins (the `#68` injection seam). The
    /// local flavor uses this to add an OS-sandboxed shell + egress-routed tools
    /// — the isolation the cloud flavor gets from its container/network sandbox
    /// instead.
    #[must_use]
    pub fn tools(mut self, provider: Arc<dyn ToolProvider>) -> Self {
        self.tool_provider = Some(provider);
        self
    }

    /// Install engine [`ToolHook`]s applied to EVERY per-turn tool registry,
    /// before the per-agent auth gate and confirmation hooks — so a host
    /// permission/surveillance hook gets first say on every call. This is the
    /// prerequisite seam for Big Smooth's narc-judge + auto-mode: it hands the
    /// server its own `ToolHook`s (an auto-mode permission gate that can
    /// allow/deny/ask, plus an LLM-judge surveillance hook) without forking the
    /// runner. Unset ⇒ no extra hooks (unchanged behavior).
    #[must_use]
    pub fn tool_hooks(mut self, hooks: Vec<Arc<dyn ToolHook>>) -> Self {
        self.tool_hooks = hooks;
        self
    }

    /// Serve the official `@smooai/smooth-operator` widget from this server: a
    /// host page at `/` and the bundle at `/chat-widget.iife.js`. `token` is
    /// injected into the page so the widget connects to this server's
    /// `/ws?token=…` (pair it with a matching [`auth`](Self::auth) verifier);
    /// pass `None` for a no-auth local server.
    #[must_use]
    pub fn serve_widget(mut self, token: Option<String>) -> Self {
        self.serve_widget = true;
        self.widget_token = token;
        self
    }

    /// Set the **default agent persona** (system prompt) for every turn that has
    /// no per-org override. A single-tenant host (the local daemon) uses this to
    /// give the agent its own personality instead of the built-in customer-support
    /// prompt. Threads to [`AppState::default_persona`](crate::state::AppState::default_persona).
    /// Unset → the built-in const prompt (unchanged).
    #[must_use]
    pub fn persona(mut self, persona: impl Into<String>) -> Self {
        self.persona = Some(persona.into());
        self
    }

    /// Serve a **host-supplied SPA** (e.g. the smooth-web dashboard) at this
    /// server's own origin, as the router fallback. The operator's explicit routes
    /// (`/ws`, `/health`, `/admin/*`) still win; everything else (`/`, hashed
    /// asset paths, SPA client routes) is served by `spa`. Use this INSTEAD of
    /// [`serve_widget`](Self::serve_widget) when the host wants its own UI at `/`
    /// — the endpoint is then simply `http://<addr>/` with no `?api`/`?token`
    /// query string (the host injects the token into the SPA's `index.html`
    /// itself, so the operator-server stays agnostic to the SPA's auth wiring).
    #[must_use]
    pub fn serve_spa(mut self, spa: axum::Router) -> Self {
        self.spa_router = Some(spa);
        self
    }

    /// Merge **host-supplied real routes** into the operator's own router, so they
    /// sit alongside `/ws`, `/health`, and `/admin/*` as first-class routes (NOT a
    /// fallback like [`serve_spa`](Self::serve_spa)). The daemon uses this to add
    /// its own endpoints — e.g. the `@`-mention `GET /search` the web composer
    /// calls — to the operator origin without the operator-server knowing about
    /// them.
    ///
    /// The supplied routes get the **same permissive CORS as `/admin`** so the
    /// cross-origin dev SPA (the Vite origin `http://localhost:3100`) can call them
    /// in the browser. The host is responsible for any auth on these routes; the
    /// operator merges them verbatim. A route here MUST NOT collide with an
    /// existing operator path (`/ws`, `/health`, `/admin/*`) — axum panics on a
    /// duplicate route at merge time.
    #[must_use]
    pub fn serve_routes(mut self, routes: axum::Router) -> Self {
        self.extra_routes = Some(routes);
        self
    }

    /// Enable **strict auth**: reject `/ws` connections with a missing/invalid
    /// token (HTTP 401) instead of degrading to an anonymous connection. Pair
    /// with [`auth`](Self::auth) — recommended whenever the server is reachable
    /// beyond loopback (e.g. a tailnet), so a tokenless peer can't drive the
    /// agent. Off by default.
    #[must_use]
    pub fn strict_auth(mut self, strict: bool) -> Self {
        self.strict_auth = strict;
        self
    }

    /// Install a **durable** storage adapter, replacing the default in-memory
    /// store. This is the seam an always-on, self-hosted deployment (the local
    /// daemon) uses to persist conversations/sessions/checkpoints across
    /// restarts without standing up Postgres — the embedder supplies any
    /// [`StorageAdapter`] (e.g. a local sqlite/dolt one). Unset → in-memory.
    #[must_use]
    pub fn storage(mut self, storage: Arc<dyn StorageAdapter>) -> Self {
        self.storage = Some(storage);
        self
    }

    /// Override the full [`ServerConfig`] (e.g. to point at a gateway / model).
    ///
    /// The local flavor still **forces** in-memory storage and the caller's bind
    /// addr regardless of what this config says — the storage/bind fields are
    /// overwritten — so the "no external services" guarantee always holds. Use
    /// this to set the gateway URL / key / model / limits for live turns.
    #[must_use]
    pub fn config(mut self, config: ServerConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// Build the [`AppState`] for the local flavor: in-memory storage +
    /// in-memory backplane (the [`build_state`] defaults) with a no-op
    /// [`NoAuthVerifier`] explicitly installed so `/admin` is reachable in-process
    /// without configuring `AUTH_MODE`. The gateway config is honored for live
    /// turns; with no key, `send_message` errors cleanly.
    fn build(&self) -> AppState {
        // Start from the caller's config (or the env-independent defaults) and
        // pin the local-flavor invariants: in-memory storage + the caller's addr.
        let mut config = self.config.clone().unwrap_or_else(local_config);
        config.storage = StorageBackend::Memory;
        config.bind = self.addr.ip().to_string();
        config.port = self.addr.port();
        config.seed_kb = self.seed_kb;

        // `build_state` gives in-memory storage + in-memory backplane + permissive
        // widget auth. Install the caller's verifier, or default to the no-op one
        // so the admin API is reachable in-process without an `AUTH_MODE=none`
        // env handshake.
        let auth = self
            .auth
            .clone()
            .unwrap_or_else(|| Arc::new(NoAuthVerifier::default()) as Arc<dyn AuthVerifier>);
        let mut state = build_state(config).with_auth(auth);
        // A durable adapter, when supplied, replaces the in-memory default — the
        // local flavor stays "no external services" but can now persist.
        if let Some(storage) = &self.storage {
            state = state.with_storage(Arc::clone(storage));
        }
        if let Some(provider) = &self.tool_provider {
            state = state.with_tools(Arc::clone(provider));
        }
        if !self.tool_hooks.is_empty() {
            state = state.with_tool_hooks(self.tool_hooks.clone());
        }
        if self.serve_widget {
            state = state.with_widget(self.widget_token.clone());
        }
        if self.strict_auth {
            state = state.with_strict_auth(true);
        }
        if let Some(persona) = &self.persona {
            state = state.with_default_persona(persona.clone());
        }
        state
    }

    /// Assemble the full axum [`Router`](axum::Router): the operator's routes
    /// (`/ws`, `/health`, `/admin/*`, and optionally the widget) plus, when a host
    /// SPA was installed via [`serve_spa`](Self::serve_spa), that SPA as the
    /// router fallback (so the explicit operator routes still win). Factored out of
    /// [`spawn`](Self::spawn) so a test can drive it with `tower::ServiceExt::oneshot`.
    fn build_app(&self) -> axum::Router {
        let mut app = router(self.build());
        // Host-supplied real routes (e.g. the daemon's `/search`) are merged so
        // they sit alongside the operator's own routes. They carry the same
        // permissive CORS as `/admin` so the cross-origin dev SPA can call them.
        if let Some(routes) = self.extra_routes.clone() {
            app = app.merge(routes.layer(crate::admin::admin_cors()));
        }
        // A host SPA (smooth-web) is mounted as the router fallback so the
        // operator's explicit routes (`/ws`, `/health`, `/admin/*`) still win and
        // everything else — `/`, hashed assets, SPA client routes — is served by
        // the SPA at this server's own origin.
        if let Some(spa) = self.spa_router.clone() {
            app = app.fallback_service(spa);
        }
        app
    }

    /// Bind and spawn the server in a background task, returning a [`LocalServer`]
    /// handle carrying the **real** bound address (resolved even for port `0`)
    /// and a graceful-shutdown switch.
    ///
    /// # Errors
    /// Returns an error if binding the TCP listener fails (e.g. the port is in
    /// use). Serving errors after a successful bind surface when the handle is
    /// awaited / shut down.
    pub async fn spawn(self) -> Result<LocalServer> {
        let listener = TcpListener::bind(self.addr)
            .await
            .with_context(|| format!("binding local smooth-operator server on {}", self.addr))?;
        let addr = listener.local_addr().context("local addr")?;

        let app = self.build_app();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let join = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    // Resolve on an explicit shutdown signal; if the sender is
                    // dropped (handle gone) we also stop, so we never leak a task.
                    let _ = shutdown_rx.await;
                })
                .await
                .context("serving local smooth-operator connections")
        });

        Ok(LocalServer {
            addr,
            shutdown_tx: Some(shutdown_tx),
            join,
        })
    }
}

/// A running [local-flavor](self) server: its bound address + a graceful
/// shutdown switch.
///
/// Dropping the handle without calling [`shutdown`](Self::shutdown) signals the
/// server to stop (the shutdown channel closes) and detaches the background
/// task. Call [`shutdown`](Self::shutdown) to stop **and** await a clean exit.
#[must_use = "the server stops when the handle is dropped; hold it for the server's lifetime"]
pub struct LocalServer {
    addr: SocketAddr,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    join: JoinHandle<Result<()>>,
}

impl LocalServer {
    /// Start building a local-flavor server. See [`LocalServerBuilder`].
    pub fn builder() -> LocalServerBuilder {
        LocalServerBuilder::default()
    }

    /// The real address the server bound on — already resolved, so this returns
    /// the concrete ephemeral port when the builder asked for port `0`.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The `ws://<addr>/ws` URL clients connect to.
    #[must_use]
    pub fn ws_url(&self) -> String {
        format!("ws://{}/ws", self.addr)
    }

    /// Signal graceful shutdown and await the server task's clean exit.
    ///
    /// # Errors
    /// Returns an error if the server task panicked or its serve loop errored.
    pub async fn shutdown(mut self) -> Result<()> {
        // Trigger graceful shutdown, then await the serve loop.
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        match (&mut self.join).await {
            Ok(result) => result,
            Err(join_err) => Err(anyhow::anyhow!("local server task failed: {join_err}")),
        }
    }
}

impl Drop for LocalServer {
    fn drop(&mut self) {
        // Best-effort: if the handle is dropped without `shutdown`, signal the
        // serve loop to stop so the background task doesn't outlive the handle.
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// An [`ServerConfig`] for the local flavor: the env-independent defaults
/// (loopback bind, default gateway URL/model/limits) with **in-memory storage**
/// pinned. The gateway URL/key are still read from the environment via
/// [`ServerConfig::from_env`] so a key present in the host's env enables live
/// turns; absent, `send_message` errors cleanly.
#[must_use]
pub fn local_config() -> ServerConfig {
    let mut config = ServerConfig::from_env();
    config.storage = StorageBackend::Memory;
    config
}

/// Run a [local-flavor](self) server to completion (blocks) on `addr`.
///
/// Convenience for the foreground / one-command case: boots a fully in-memory,
/// auth-off server bound to `addr` and serves until the process is killed. For
/// an embedded server you can stop programmatically, use
/// [`LocalServer::builder`] + [`LocalServer::shutdown`] instead.
///
/// # Errors
/// Returns an error if `addr` doesn't parse, the bind fails, or serving fails.
pub async fn serve_local(addr: &str) -> Result<()> {
    let addr: SocketAddr = addr
        .parse()
        .with_context(|| format!("parsing local bind address '{addr}'"))?;
    let server = LocalServer::builder().addr(addr).spawn().await?;
    let local = server.addr();
    println!("smooth-operator-server (local flavor) listening on ws://{local}/ws");
    tracing::info!(%local, endpoint = "/ws", "smooth-operator-server (local flavor) listening");

    // Take ownership of the join handle and await it to completion. We can't
    // call the consuming `shutdown` here (we want to run forever), so await the
    // task directly via a small accessor.
    server.run_to_completion().await
}

impl LocalServer {
    /// Await the server task to completion (blocks). Used by [`serve_local`] for
    /// the run-forever foreground case. The handle is consumed; graceful
    /// shutdown then only happens on process exit / task error.
    async fn run_to_completion(mut self) -> Result<()> {
        // Keep the shutdown sender alive (don't fire it) so the server runs until
        // the task itself ends (process killed / serve error).
        match (&mut self.join).await {
            Ok(result) => result,
            Err(join_err) => Err(anyhow::anyhow!("local server task failed: {join_err}")),
        }
        // `self` (and thus `shutdown_tx`) drops here; the loop is already done.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spawn_binds_ephemeral_and_reports_real_addr() {
        // Port 0 → the OS picks a port; the handle must report the real one.
        let server = LocalServer::builder()
            .addr("127.0.0.1:0".parse().unwrap())
            .spawn()
            .await
            .expect("spawn local server");
        let addr = server.addr();
        assert_ne!(addr.port(), 0, "ephemeral port must be resolved: {addr}");
        assert!(server.ws_url().starts_with("ws://127.0.0.1:"));
        server.shutdown().await.expect("clean shutdown");
    }

    #[tokio::test]
    async fn build_uses_in_memory_storage_and_no_auth() {
        let state = LocalServerBuilder::default()
            .config(ServerConfig {
                // Even if a caller hands a Postgres config, the local flavor pins
                // in-memory so the no-external-services guarantee holds.
                storage: StorageBackend::Postgres,
                ..local_config()
            })
            .build();
        assert_eq!(state.config.storage, StorageBackend::Memory);
        // The no-op verifier is installed (admin reachable in-process).
        assert_eq!(state.auth.mode(), "none");
    }

    #[test]
    fn storage_seam_installs_a_durable_adapter() {
        use smooth_operator_adapter_memory::InMemoryStorageAdapter;
        // Any StorageAdapter stands in for a durable one; assert the builder
        // installs the *injected* adapter, not the hardcoded in-memory default.
        let injected: Arc<dyn StorageAdapter> = Arc::new(InMemoryStorageAdapter::new());
        let state = LocalServerBuilder::default()
            .storage(Arc::clone(&injected))
            .build();
        assert!(
            Arc::ptr_eq(&state.storage, &injected),
            "the injected storage adapter must be installed"
        );
        // Default (no override) → a distinct in-memory adapter.
        let default_state = LocalServerBuilder::default().build();
        assert!(!Arc::ptr_eq(&default_state.storage, &injected));
    }

    #[test]
    fn auth_seam_installs_a_custom_verifier() {
        use smooth_operator::auth::LocalTokenVerifier;
        let state = LocalServerBuilder::default()
            .auth(Arc::new(LocalTokenVerifier::new("s3cret")))
            .build();
        assert_eq!(
            state.auth.mode(),
            "local-token",
            "custom verifier overrides the default"
        );
    }

    #[test]
    fn tools_seam_installs_a_provider() {
        use async_trait::async_trait;
        use smooth_operator::tool_provider::{ToolProvider, ToolProviderContext};
        use smooth_operator_core::Tool;

        struct EmptyProvider;
        #[async_trait]
        impl ToolProvider for EmptyProvider {
            async fn tools_for(&self, _ctx: &ToolProviderContext) -> Vec<Arc<dyn Tool>> {
                Vec::new()
            }
        }
        let state = LocalServerBuilder::default()
            .tools(Arc::new(EmptyProvider))
            .build();
        assert!(state.tool_provider.is_some(), "host ToolProvider installed");
    }

    #[test]
    fn tool_hooks_seam_installs_hooks() {
        use async_trait::async_trait;
        use smooth_operator_core::tool::ToolHook;

        struct NoopHook;
        #[async_trait]
        impl ToolHook for NoopHook {}

        // Default → no hooks (unchanged behavior).
        assert!(
            LocalServerBuilder::default().build().tool_hooks.is_empty(),
            "no tool hooks unless the host installs them"
        );
        // `.tool_hooks(..)` threads the hooks onto AppState for the runner.
        let state = LocalServerBuilder::default()
            .tool_hooks(vec![Arc::new(NoopHook) as Arc<dyn ToolHook>])
            .build();
        assert_eq!(
            state.tool_hooks.len(),
            1,
            "the installed hook must reach AppState"
        );
    }

    #[test]
    fn serve_widget_opts_into_the_widget_routes_with_token() {
        let state = LocalServerBuilder::default()
            .serve_widget(Some("tok-123".into()))
            .build();
        assert!(state.serve_widget, "widget routes opted in");
        assert_eq!(state.widget_token.as_deref(), Some("tok-123"));
        // Building the router with serve_widget set mounts `/` + the bundle route.
        let _ = crate::server::router(state);
    }

    #[test]
    fn no_widget_by_default() {
        let state = LocalServerBuilder::default().build();
        assert!(
            !state.serve_widget,
            "widget off by default (K8s/Lambda never serve it)"
        );
        assert_eq!(state.widget_token, None);
    }

    #[test]
    fn persona_seam_installs_default_persona() {
        // No persona → no default (built-in const prompt, unchanged behavior).
        assert_eq!(
            LocalServerBuilder::default().build().default_persona,
            None,
            "no default persona unless set"
        );
        // `.persona(..)` threads through to AppState::default_persona.
        let state = LocalServerBuilder::default()
            .persona("You are Big Smooth.")
            .build();
        assert_eq!(
            state.default_persona.as_deref(),
            Some("You are Big Smooth.")
        );
    }

    #[tokio::test]
    async fn serve_spa_mounts_host_router_as_fallback() {
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        // A trivial host SPA: any unmatched path returns a sentinel. The
        // operator's `/health` must still win (an explicit route beats the SPA
        // fallback).
        let spa = axum::Router::new().fallback(axum::routing::get(|| async { "SPA-ROOT" }));
        let app = LocalServerBuilder::default().serve_spa(spa).build_app();

        // `/` (and any non-operator path) routes to the SPA.
        let res = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::OK);
        let body = res.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            &body[..],
            b"SPA-ROOT",
            "the host SPA is served as the fallback"
        );

        // The operator's explicit `/health` route still wins over the SPA fallback.
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/health")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::OK);
        let body = res.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            &body[..],
            b"ok",
            "explicit operator routes win over the SPA"
        );
    }

    #[test]
    fn no_spa_by_default() {
        // Without `serve_spa`, an unknown path is a 404 (no fallback service).
        assert!(
            LocalServerBuilder::default().spa_router.is_none(),
            "no SPA mounted unless the host installs one"
        );
    }

    #[tokio::test]
    async fn serve_routes_merges_host_routes_alongside_operator_routes() {
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        // A host route that must respond as a real route (not a fallback), while
        // the operator's own `/health` keeps working.
        let routes =
            axum::Router::new().route("/search", axum::routing::get(|| async { "SEARCH-OK" }));
        let app = LocalServerBuilder::default()
            .serve_routes(routes)
            .build_app();

        // The merged host route responds.
        let res = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/search?q=foo")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::OK);
        let body = res.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"SEARCH-OK", "merged host route responds");

        // The operator's own `/health` still works alongside the merged routes.
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/health")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::OK);
        let body = res.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"ok", "operator routes survive the merge");
    }

    #[test]
    fn no_extra_routes_by_default() {
        assert!(
            LocalServerBuilder::default().extra_routes.is_none(),
            "no host routes merged unless the host installs them"
        );
    }

    #[test]
    fn strict_auth_off_by_default_and_opt_in() {
        assert!(
            !LocalServerBuilder::default().build().strict_auth,
            "lenient/anonymous by default"
        );
        assert!(
            LocalServerBuilder::default()
                .strict_auth(true)
                .build()
                .strict_auth,
            "opt-in threads to AppState"
        );
    }
}
