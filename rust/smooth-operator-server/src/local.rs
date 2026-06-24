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

use smooth_operator::auth::NoAuthVerifier;

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
#[derive(Debug, Clone)]
pub struct LocalServerBuilder {
    addr: SocketAddr,
    seed_kb: bool,
    config: Option<ServerConfig>,
}

impl Default for LocalServerBuilder {
    fn default() -> Self {
        Self {
            addr: DEFAULT_LOCAL_ADDR
                .parse()
                .expect("DEFAULT_LOCAL_ADDR is a valid SocketAddr"),
            seed_kb: false,
            config: None,
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
        // widget auth. Install the no-op verifier explicitly so the admin API is
        // reachable in-process without an `AUTH_MODE=none` env handshake.
        build_state(config).with_auth(Arc::new(NoAuthVerifier::default()))
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

        let app = router(self.build());
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
}
