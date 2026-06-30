//! Server + per-connection state.
//!
//! [`AppState`] is shared across every connection + every admin HTTP request
//! (cloneable `Arc` handles): the storage adapter, the resolved
//! [`ServerConfig`], the session registry, and — for the admin API (Phase 12) —
//! the [`AuthVerifier`], an [`IndexingStore`], and the document-set registry.
//!
//! Sessions live in an in-memory map keyed by `sessionId` so `get_session` and
//! reconnects work across connections (mirrors the protocol's "connection →
//! session" / "session → connections" state model, simplified for the reference
//! single-process server). On AWS this map would be DynamoDB; on k8s, Redis or
//! Postgres.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use smooth_operator_core::HumanResponse;
use tokio::sync::mpsc::UnboundedSender;

use smooth_operator::adapter::StorageAdapter;
use smooth_operator::auth::{AuthVerifier, NoAuthVerifier};
use smooth_operator::backplane::{Backplane, InMemoryBackplane};
use smooth_operator::connector_config::{ConnectorConfigStore, InMemoryConnectorConfigStore};
use smooth_operator::domain::Session;
use smooth_operator::gateway_key::{EnvGatewayKeyResolver, GatewayKeyResolver};
use smooth_operator::settings::{InMemorySettingsStore, SettingsStore};
use smooth_operator::tool_provider::ToolProvider;
use smooth_operator::widget_auth::{PermissiveWidgetAuth, WidgetAuthProvider};
use tokio_util::sync::CancellationToken;

use smooth_operator_core::llm_provider::LlmProvider;
use smooth_operator_ingestion::indexing::{InMemoryIndexingStore, IndexingStore};

use crate::config::ServerConfig;

/// Shared, cloneable application state handed to every WebSocket connection +
/// every admin HTTP request.
#[derive(Clone)]
pub struct AppState {
    /// The single storage seam (conversations / participants / messages /
    /// sessions / checkpoints / knowledge).
    pub storage: Arc<dyn StorageAdapter>,
    /// Resolved server configuration (gateway, model, limits).
    pub config: Arc<ServerConfig>,
    /// The configured auth verifier (jwt / smoo / none). Used by the admin API's
    /// `require_role` extractor to turn a bearer token into a `Principal`.
    pub auth: Arc<dyn AuthVerifier>,
    /// Indexing-run status store, surfaced by `GET /admin/indexing/runs`.
    pub indexing: Arc<dyn IndexingStore>,
    /// Connector-configuration store, CRUD'd by the admin write API
    /// (`/admin/connectors`). Org-scoped; holds an `auth_ref` (secret name), not
    /// the secret itself.
    pub connector_configs: Arc<dyn ConnectorConfigStore>,
    /// Per-org agent settings store, read/written by `/admin/settings`.
    pub settings: Arc<dyn SettingsStore>,
    /// **Host tool-injection seam.** When `Some`, the runner asks this provider
    /// for EXTRA tools and merges them into every turn's `ToolRegistry`
    /// alongside the built-ins. Defaults to `None` (built-ins only); a host
    /// installs one via [`with_tools`](Self::with_tools) to contribute its own
    /// per-org tool catalog without forking the runner.
    pub tool_provider: Option<Arc<dyn ToolProvider>>,
    /// Embeddable-widget auth hook: resolves an agent's origin-allowlist +
    /// public-key policy for `<smooth-agent-chat>` connections. Defaults to
    /// [`PermissiveWidgetAuth`] (no enforcement) until a host installs a real
    /// provider via [`with_widget_auth`](Self::with_widget_auth).
    pub widget_auth: Arc<dyn WidgetAuthProvider>,
    /// Connection backplane: per-pod sink registry + cross-pod event delivery.
    /// Defaults to [`InMemoryBackplane`] (single-process); a host installs a
    /// Redis/NATS impl via [`with_backplane`](Self::with_backplane) to scale out
    /// and to let non-AI publishers push realtime events to connected clients.
    pub backplane: Arc<dyn Backplane>,
    /// Test-only injected LLM surface. When `Some`, every `send_message` turn
    /// runs the engine against this provider (a
    /// [`MockLlmClient`](smooth_operator_core::llm_provider::MockLlmClient))
    /// instead of building a live gateway client from `config` — exactly the
    /// `ServerState(chat_client=mock)` seam the Python reference uses to drive the
    /// scenario-parity corpus deterministically offline. **`None` in production**
    /// (a live client is built from the gateway config), so the `/ws` path is
    /// byte-for-byte unchanged for real deployments. Installed via
    /// [`with_chat_provider`](Self::with_chat_provider).
    pub chat_provider: Option<Arc<dyn LlmProvider>>,
    /// Per-org LLM gateway-key resolver: maps a turn's `org_id` to the gateway
    /// key it should bill/scope to. Defaults to [`EnvGatewayKeyResolver`] (the
    /// single `SMOOAI_GATEWAY_KEY` for every org — unchanged local behavior); a
    /// multi-tenant host installs a per-org resolver via
    /// [`with_gateway_key_resolver`](Self::with_gateway_key_resolver) so each
    /// tenant's usage is attributed to its own key. The per-turn LLM-config build
    /// falls back to the env key whenever the resolver returns `None`.
    pub gateway_key_resolver: Arc<dyn GatewayKeyResolver>,
    /// Graceful-shutdown signal, shared across every per-connection clone of this
    /// state. On SIGTERM/ctrl_c the serve loop cancels this token; each
    /// connection's reader loop selects on [`CancellationToken::cancelled`] so it
    /// finishes its in-flight turn, exits, and detaches from the [`Backplane`] —
    /// no in-flight turn dropped, no stale registry entry left behind. A fresh
    /// token from [`new`](Self::new) is never cancelled, so the `/ws` path and
    /// tests are unaffected until a `run`/serve path wires the signal.
    pub shutdown: CancellationToken,
    /// Session registry: `sessionId` → session blob. Shared across connections.
    sessions: Arc<RwLock<HashMap<String, Session>>>,
    /// Document-set registry, **org-scoped**: `org_id` → (set name → document
    /// count). The in-memory knowledge backend drops document metadata on
    /// ingest, so the admin API reads document-set membership from this side
    /// registry. Keyed by org so org A's document sets are never reported to an
    /// org-B caller (cross-org leak fix — SMOODEV access-control hardening).
    doc_sets: Arc<RwLock<HashMap<String, HashMap<String, usize>>>>,
    /// Connector registry, **org-scoped**: `org_id` → set of connector names
    /// whose indexing runs should be listed. Keyed by org so a same-named
    /// connector in two orgs does not collide, and `GET /admin/indexing/runs`
    /// only ever lists the caller's org's connectors.
    connectors: Arc<RwLock<HashMap<String, Vec<String>>>>,
    /// **Human-in-the-loop pending confirmations**: `sessionId` →
    /// [`HumanResponse`] sender for a turn currently parked on a write-tool
    /// confirmation. When an agent turn calls a tool that requires human
    /// approval, the runner installs a `ConfirmationHook` (smooth-operator-core)
    /// that parks the loop and registers its response sender here. A subsequent
    /// `confirm_tool_action` frame looks the session up, takes the sender, and
    /// feeds it [`HumanResponse::Approved`] / [`HumanResponse::Denied`] to resume
    /// the parked turn (execute or reject the tool). Keyed by session so each
    /// session has at most one outstanding confirmation; an empty map means no
    /// turn is parked (the default, byte-for-byte unchanged from before HITL).
    pending_confirmations: Arc<RwLock<HashMap<String, UnboundedSender<HumanResponse>>>>,
    /// When `true`, the router mounts the embedded widget host page at `/` and
    /// the widget bundle at `/chat-widget.iife.js`. Off by default (the
    /// K8s/Lambda flavors never serve the widget); the local flavor opts in via
    /// [`with_widget`](Self::with_widget).
    pub serve_widget: bool,
    /// The auth token injected into the served widget host page (same-origin), so
    /// the embedded widget connects to this server's `/ws?token=…`. `None` ⇒ no
    /// token injected (a no-auth local server).
    pub widget_token: Option<String>,
    /// **Strict auth.** When `true`, the `/ws` connect path **rejects** a
    /// missing/invalid token (HTTP 401) instead of degrading to an anonymous
    /// connection. Off by default (K8s/widget anonymous flows unchanged); a
    /// single-tenant local/tailnet deployment opts in via
    /// [`with_strict_auth`](Self::with_strict_auth) so a tokenless peer can't
    /// drive the agent.
    pub strict_auth: bool,
    /// **Default agent persona / system prompt.** When `Some`, it is used as the
    /// turn's system prompt whenever the per-org [`AgentSettings::persona`] is
    /// `None` — i.e. a host-supplied default that replaces the built-in
    /// customer-support [`KNOWLEDGE_CHAT_SYSTEM_PROMPT`](crate::runner) when no
    /// per-org override exists. The single-tenant local daemon installs its
    /// "Big Smooth" personal-assistant persona here via
    /// [`with_default_persona`](Self::with_default_persona). `None` (the default)
    /// keeps the const prompt, so the cloud flavor is byte-for-byte unchanged.
    pub default_persona: Option<String>,
    /// **Model-pricing cache** for `GET /admin/model-costs`. The gateway's
    /// `/v1/model/info` pricing is stable, so it's fetched at most once per
    /// process and reused for every subsequent request (the admin handler sets
    /// this on the first successful fetch; a gateway error is NOT cached, so a
    /// transient failure is retried on the next request). Shared across clones so
    /// every connection/request sees the same cached map.
    pub model_costs_cache: Arc<tokio::sync::OnceCell<serde_json::Value>>,
}

/// Namespace a connector name by org for the [`IndexingStore`] key, so two orgs
/// with a same-named connector (`"docs"`) record + list **separate** runs. The
/// `\u{1}` separator can't appear in a user-supplied connector name, so it can't
/// be spoofed to cross an org boundary.
#[must_use]
pub fn scoped_connector_key(org_id: &str, connector_name: &str) -> String {
    format!("IXCONN#{org_id}\u{1}{connector_name}")
}

impl AppState {
    /// Construct shared state over a storage adapter and config.
    ///
    /// Defaults the admin-API collaborators: a [`NoAuthVerifier`] (overridden via
    /// [`with_auth`](Self::with_auth)) and an empty [`InMemoryIndexingStore`]
    /// (overridden via [`with_indexing`](Self::with_indexing)). The `/ws` path
    /// uses none of these, so existing callers are unaffected.
    #[must_use]
    pub fn new(storage: Arc<dyn StorageAdapter>, config: ServerConfig) -> Self {
        // Default resolver returns the single env gateway key for every org, so
        // the local/default flavor is unchanged until a host installs a per-org
        // resolver via `with_gateway_key_resolver`.
        let gateway_key_resolver: Arc<dyn GatewayKeyResolver> =
            Arc::new(EnvGatewayKeyResolver::new(config.gateway_key.clone()));
        Self {
            storage,
            config: Arc::new(config),
            auth: Arc::new(NoAuthVerifier::default()),
            indexing: Arc::new(InMemoryIndexingStore::new()),
            connector_configs: Arc::new(InMemoryConnectorConfigStore::new()),
            settings: Arc::new(InMemorySettingsStore::new()),
            tool_provider: None,
            widget_auth: Arc::new(PermissiveWidgetAuth),
            backplane: Arc::new(InMemoryBackplane::new()),
            chat_provider: None,
            gateway_key_resolver,
            // A fresh, never-cancelled token: every clone of this state shares
            // its cancellation state, so the serve loop cancelling once fans out
            // to every connection. Defaulting here (rather than at each call
            // site) keeps construction ripple-free.
            shutdown: CancellationToken::new(),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            doc_sets: Arc::new(RwLock::new(HashMap::new())),
            connectors: Arc::new(RwLock::new(HashMap::new())),
            pending_confirmations: Arc::new(RwLock::new(HashMap::new())),
            serve_widget: false,
            widget_token: None,
            strict_auth: false,
            default_persona: None,
            model_costs_cache: Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    /// Install the configured auth verifier (builder).
    #[must_use]
    pub fn with_auth(mut self, auth: Arc<dyn AuthVerifier>) -> Self {
        self.auth = auth;
        self
    }

    /// Replace the storage adapter (builder).
    ///
    /// Lets an embedder (e.g. the local-flavor daemon) swap the default
    /// in-memory store for a **durable local adapter** — the seam an always-on,
    /// self-hosted deployment needs so conversations/sessions/checkpoints
    /// survive a restart without standing up Postgres.
    #[must_use]
    pub fn with_storage(mut self, storage: Arc<dyn StorageAdapter>) -> Self {
        self.storage = storage;
        self
    }

    /// Install the indexing store (builder).
    #[must_use]
    pub fn with_indexing(mut self, indexing: Arc<dyn IndexingStore>) -> Self {
        self.indexing = indexing;
        self
    }

    /// Install the connector-configuration store (builder).
    #[must_use]
    pub fn with_connector_configs(mut self, store: Arc<dyn ConnectorConfigStore>) -> Self {
        self.connector_configs = store;
        self
    }

    /// Install the agent-settings store (builder).
    #[must_use]
    pub fn with_settings(mut self, store: Arc<dyn SettingsStore>) -> Self {
        self.settings = store;
        self
    }

    /// Install a host [`ToolProvider`] (builder). The runner merges the
    /// provider's per-turn tools into every turn's registry alongside the
    /// built-ins. Without this, the registry is exactly the built-ins, so the
    /// default/local flavor is unaffected.
    #[must_use]
    pub fn with_tools(mut self, provider: Arc<dyn ToolProvider>) -> Self {
        self.tool_provider = Some(provider);
        self
    }

    /// Enable **strict auth** (builder): reject `/ws` connections with a
    /// missing/invalid token (HTTP 401) instead of degrading to anonymous. Pair
    /// with a real [`with_auth`](Self::with_auth) verifier. Off by default.
    #[must_use]
    pub fn with_strict_auth(mut self, strict: bool) -> Self {
        self.strict_auth = strict;
        self
    }

    /// Install a **default agent persona** (builder): the system prompt used for
    /// a turn when the per-org [`AgentSettings::persona`] is unset. A single-tenant
    /// host (the local daemon) installs its own personality here so every turn
    /// runs as that agent rather than the built-in customer-support prompt. `None`
    /// (the default) keeps the const prompt, so the cloud flavor is unchanged. An
    /// empty/whitespace-only string is treated as no default.
    #[must_use]
    pub fn with_default_persona(mut self, persona: impl Into<String>) -> Self {
        let persona = persona.into();
        self.default_persona = if persona.trim().is_empty() {
            None
        } else {
            Some(persona)
        };
        self
    }

    /// Serve the embedded official widget (host page at `/`, bundle at
    /// `/chat-widget.iife.js`), injecting `token` into the page so the widget
    /// connects to this server's `/ws?token=…` (builder). The local deployment
    /// flavor opts in; other flavors never mount the widget routes.
    #[must_use]
    pub fn with_widget(mut self, token: Option<String>) -> Self {
        self.serve_widget = true;
        self.widget_token = token;
        self
    }

    /// Install the embeddable-widget auth provider (builder). A host backs this
    /// with its agent store so embed origins + public keys are enforced.
    #[must_use]
    pub fn with_widget_auth(mut self, provider: Arc<dyn WidgetAuthProvider>) -> Self {
        self.widget_auth = provider;
        self
    }

    /// Install the connection backplane (builder). A host installs a Redis/NATS
    /// impl to scale the WS service horizontally and to let other services push
    /// realtime events to connected clients via [`Backplane::publish`].
    #[must_use]
    pub fn with_backplane(mut self, backplane: Arc<dyn Backplane>) -> Self {
        self.backplane = backplane;
        self
    }

    /// Install a test-injected LLM provider (builder). Every `send_message` turn
    /// then runs the engine against this provider instead of a live gateway
    /// client — the [`MockLlmClient`](smooth_operator_core::llm_provider::MockLlmClient)
    /// seam the scenario-parity corpus drives. Production never calls this, so the
    /// live path is unchanged. See [`chat_provider`](Self::chat_provider).
    #[must_use]
    pub fn with_chat_provider(mut self, provider: Arc<dyn LlmProvider>) -> Self {
        self.chat_provider = Some(provider);
        self
    }

    /// Install a per-org gateway-key resolver (builder). A multi-tenant host
    /// installs a resolver backed by its per-org key store (e.g. one LiteLLM
    /// virtual key per tenant) so each org's turns are billed/scoped to its own
    /// key. The per-turn LLM-config build falls back to the env key whenever the
    /// resolver returns `None`, so a resolver covering only some orgs is safe.
    /// Leaving this unset keeps the default [`EnvGatewayKeyResolver`] (single env
    /// key for every org — unchanged local behavior).
    #[must_use]
    pub fn with_gateway_key_resolver(mut self, resolver: Arc<dyn GatewayKeyResolver>) -> Self {
        self.gateway_key_resolver = resolver;
        self
    }

    /// Install the graceful-shutdown signal (builder). The serve loop owns a
    /// clone of this token and cancels it on SIGTERM/ctrl_c; every per-connection
    /// clone observes the cancellation and drains. Defaulted to a fresh token in
    /// [`new`](Self::new), so this is only needed when a caller wants to drive
    /// shutdown from its own token.
    #[must_use]
    pub fn with_shutdown(mut self, shutdown: CancellationToken) -> Self {
        self.shutdown = shutdown;
        self
    }

    /// Register a freshly created session.
    pub fn insert_session(&self, session: Session) {
        if let Ok(mut map) = self.sessions.write() {
            map.insert(session.session_id.clone(), session);
        }
    }

    /// Look up a session by id.
    #[must_use]
    pub fn get_session(&self, session_id: &str) -> Option<Session> {
        self.sessions.read().ok()?.get(session_id).cloned()
    }

    /// Record that a document was added to a named document set **within an org**
    /// (increments its count). Used by seeding + the ingest path so
    /// `GET /admin/document-sets` can report set names + counts despite the
    /// in-memory backend dropping document metadata. Org-scoped so org A's sets
    /// are never reported to an org-B caller.
    pub fn record_document_set(&self, org_id: impl Into<String>, set: impl Into<String>) {
        if let Ok(mut map) = self.doc_sets.write() {
            *map.entry(org_id.into())
                .or_default()
                .entry(set.into())
                .or_insert(0) += 1;
        }
    }

    /// Snapshot **one org's** document-set registry as `(name, count)` pairs,
    /// sorted by name for a stable response. Never returns another org's sets.
    #[must_use]
    pub fn document_sets(&self, org_id: &str) -> Vec<(String, usize)> {
        let Ok(map) = self.doc_sets.read() else {
            return Vec::new();
        };
        let Some(org_sets) = map.get(org_id) else {
            return Vec::new();
        };
        let mut out: Vec<(String, usize)> = org_sets.iter().map(|(k, v)| (k.clone(), *v)).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Record a connector (within an org) whose indexing runs should be listed
    /// (idempotent). Org-scoped so a same-named connector in two orgs records
    /// separately and `GET /admin/indexing/runs` only lists the caller's org's.
    pub fn record_connector(&self, org_id: impl Into<String>, name: impl Into<String>) {
        let name = name.into();
        if let Ok(mut map) = self.connectors.write() {
            let v = map.entry(org_id.into()).or_default();
            if !v.iter().any(|c| c == &name) {
                v.push(name);
            }
        }
    }

    /// Snapshot **one org's** recorded connector names (sorted, stable). Never
    /// returns another org's connectors.
    #[must_use]
    pub fn connectors(&self, org_id: &str) -> Vec<String> {
        let Ok(map) = self.connectors.read() else {
            return Vec::new();
        };
        let mut out = map.get(org_id).cloned().unwrap_or_default();
        out.sort();
        out
    }

    /// Register a parked turn's [`HumanResponse`] sender for `session_id`, so a
    /// later `confirm_tool_action` can resume it. Any prior pending sender for
    /// the same session is replaced (one outstanding confirmation per session).
    /// Called by the runner's confirmation bridge when a write tool emits a
    /// `HumanRequest::Confirm`.
    pub fn register_confirmation(
        &self,
        session_id: impl Into<String>,
        responder: UnboundedSender<HumanResponse>,
    ) {
        if let Ok(mut map) = self.pending_confirmations.write() {
            map.insert(session_id.into(), responder);
        }
    }

    /// Take (remove + return) the pending [`HumanResponse`] sender for
    /// `session_id`, if a turn is parked on a confirmation. Returns `None` when
    /// no turn awaits confirmation for that session (the common case). Taking it
    /// out — rather than cloning — guarantees a single confirmation resolves a
    /// single parked tool call, and a duplicate `confirm_tool_action` is a no-op.
    #[must_use]
    pub fn take_confirmation(&self, session_id: &str) -> Option<UnboundedSender<HumanResponse>> {
        self.pending_confirmations.write().ok()?.remove(session_id)
    }

    /// Drop any pending confirmation registered for `session_id` without
    /// resolving it. Called when a parked turn ends (the bridge task finishes)
    /// so a stale sender can't linger and mis-route a later confirmation.
    pub fn clear_confirmation(&self, session_id: &str) {
        if let Ok(mut map) = self.pending_confirmations.write() {
            map.remove(session_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use smooth_operator::gateway_key::resolve_gateway_key;
    use smooth_operator_adapter_memory::InMemoryStorageAdapter;

    use crate::config::{ServerConfig, StorageBackend, DEFAULT_GATEWAY_URL, DEFAULT_MODEL};

    /// Build a config with an explicit env gateway key for the resolver tests.
    fn config_with_env_key(env_key: Option<&str>) -> ServerConfig {
        ServerConfig {
            bind: "127.0.0.1".to_string(),
            port: 0,
            gateway_url: DEFAULT_GATEWAY_URL.to_string(),
            gateway_key: env_key.map(str::to_string),
            model: DEFAULT_MODEL.to_string(),
            seed_kb: false,
            max_iterations: 6,
            max_tokens: 512,
            storage: StorageBackend::Memory,
            widget_auth_strict: false,
            confirm_tools: Vec::new(),
        }
    }

    fn state_with(config: ServerConfig) -> AppState {
        AppState::new(Arc::new(InMemoryStorageAdapter::new()), config)
    }

    #[test]
    fn default_persona_unset_by_default() {
        let state = state_with(config_with_env_key(None));
        assert_eq!(
            state.default_persona, None,
            "no default persona unless a host installs one"
        );
    }

    #[test]
    fn with_default_persona_installs_and_trims_empty() {
        let state =
            state_with(config_with_env_key(None)).with_default_persona("You are Big Smooth.");
        assert_eq!(
            state.default_persona.as_deref(),
            Some("You are Big Smooth.")
        );
        // An empty / whitespace-only persona is treated as "no default".
        let blank = state_with(config_with_env_key(None)).with_default_persona("   ");
        assert_eq!(blank.default_persona, None, "blank persona is ignored");
    }

    /// Per-org resolver covering exactly one org; `None` (→ env fallback) for any
    /// other org. Mirrors what a multi-tenant host installs.
    struct OneOrgResolver {
        org: String,
        key: String,
    }

    #[async_trait]
    impl GatewayKeyResolver for OneOrgResolver {
        async fn resolve(&self, org_id: &str) -> Option<String> {
            (org_id == self.org).then(|| self.key.clone())
        }
    }

    #[tokio::test]
    async fn default_state_resolves_env_key_for_every_org() {
        // No resolver injected: the default `EnvGatewayKeyResolver` returns the
        // single env key for every org — unchanged local behavior.
        let state = state_with(config_with_env_key(Some("env-key")));
        let env = state.config.gateway_key.as_deref();
        assert_eq!(
            resolve_gateway_key(&state.gateway_key_resolver, "org-a", env).await,
            Some("env-key".to_string())
        );
        assert_eq!(
            resolve_gateway_key(&state.gateway_key_resolver, "org-z", env).await,
            Some("env-key".to_string())
        );
    }

    #[tokio::test]
    async fn injected_resolver_overrides_per_org_and_falls_back_to_env() {
        let config = config_with_env_key(Some("env-key"));
        let state = state_with(config).with_gateway_key_resolver(Arc::new(OneOrgResolver {
            org: "org-a".to_string(),
            key: "org-a-key".to_string(),
        }));
        let env = state.config.gateway_key.as_deref();

        // Covered org → its own key.
        assert_eq!(
            resolve_gateway_key(&state.gateway_key_resolver, "org-a", env).await,
            Some("org-a-key".to_string())
        );
        // Uncovered org → env fallback.
        assert_eq!(
            resolve_gateway_key(&state.gateway_key_resolver, "org-b", env).await,
            Some("env-key".to_string())
        );
    }

    #[tokio::test]
    async fn no_env_key_and_no_resolver_match_resolves_to_none() {
        // Env key absent + default resolver → no key (turn is unavailable). Same
        // behavior as today's `llm_config()` returning `None`.
        let state = state_with(config_with_env_key(None));
        let env = state.config.gateway_key.as_deref();
        assert_eq!(
            resolve_gateway_key(&state.gateway_key_resolver, "org-a", env).await,
            None
        );
    }
}
