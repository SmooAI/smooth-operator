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
use smooth_operator::agent_config::{AgentConfigResolver, StaticAgentConfigResolver};
use smooth_operator::auth::{AuthVerifier, NoAuthVerifier};
use smooth_operator::backplane::{Backplane, InMemoryBackplane};
use smooth_operator::connector_config::{ConnectorConfigStore, InMemoryConnectorConfigStore};
use smooth_operator::domain::Session;
use smooth_operator::gateway_key::{EnvGatewayKeyResolver, GatewayKeyResolver};
use smooth_operator::identity_intake::IntakeValues;
use smooth_operator::interaction::{InteractionOutcome, InteractionRegistry};
use smooth_operator::otp::{OtpContact, OtpService};
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
    /// **Per-agent behavior config hook.** Resolves an agent's `instructions`
    /// (system prompt), `personality`, `greeting`, and `conversation_workflow`
    /// from its `agent_id` so a public chat agent behaves as its owner configured
    /// — not as the generic org-default persona. Defaults to
    /// [`StaticAgentConfigResolver`](smooth_operator::agent_config::StaticAgentConfigResolver) (empty ⇒ no
    /// per-agent config → the org default persona is used, unchanged); a host
    /// installs a real provider (backed by the monorepo `agents` table) via
    /// [`with_agent_config`](Self::with_agent_config).
    pub agent_config: Arc<dyn AgentConfigResolver>,
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
    /// **Rich Interactions kind catalog.** The interaction kinds this server
    /// hosts (raise tool + validator + conversational fallback per kind — see
    /// `smooth_operator::interaction`). Defaults to the reference catalog
    /// (`identity_intake`); a host may extend it via
    /// [`with_interactions`](Self::with_interactions).
    pub interactions: Arc<InteractionRegistry>,
    /// **End-user OTP identity-verification seam.** When `Some`, a turn whose
    /// auth gate refuses an `end_user` tool on an unverified session triggers the
    /// OTP flow: the server emits `otp_verification_required`, calls
    /// [`send_otp`](smooth_operator::otp::OtpService::send_otp), and emits
    /// `otp_sent`; a later `verify_otp` action calls
    /// [`verify_otp`](smooth_operator::otp::OtpService::verify_otp) and, on
    /// success, marks the session authenticated. `None` (the default) keeps the
    /// current fail-closed behavior — the `end_user` tool is refused and no OTP is
    /// offered. Installed via [`with_otp_service`](Self::with_otp_service). The
    /// reference server never holds a code; the host owns generation/expiry.
    pub otp_service: Option<Arc<dyn OtpService>>,
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
    /// **Rich Interactions pending parks**: `sessionId` → the parked turn's
    /// interaction (id + kind + spec) and [`InteractionOutcome`] sender. When an
    /// agent turn's raise tool parks on a capability-declaring session, the
    /// runner's interaction bridge registers here; a subsequent
    /// `submit_interaction` frame validates against the registered kind + spec,
    /// then takes the sender and feeds it the outcome (submitted values or a
    /// decline) to resume the parked turn. One outstanding interaction per
    /// session (mirrors `pending_confirmations`).
    pending_interactions: Arc<RwLock<HashMap<String, PendingInteraction>>>,
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

/// A turn parked on a Rich Interaction: the interaction instance (id + kind +
/// spec — the validation contract for the incoming `submit_interaction`) and
/// the sender that resumes it.
#[derive(Clone)]
pub struct PendingInteraction {
    /// Server-generated id for this interaction instance; the submit must echo
    /// it so a stale submit can never resolve a newer park.
    pub interaction_id: String,
    /// The interaction kind (routes to its validator).
    pub kind: String,
    /// The kind-specific spec the raise carried (drives validation).
    pub spec: serde_json::Value,
    /// Resumes the parked raise tool.
    pub responder: UnboundedSender<InteractionOutcome>,
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
            agent_config: Arc::new(StaticAgentConfigResolver::default()),
            backplane: Arc::new(InMemoryBackplane::new()),
            chat_provider: None,
            gateway_key_resolver,
            otp_service: None,
            interactions: Arc::new(InteractionRegistry::default()),
            // A fresh, never-cancelled token: every clone of this state shares
            // its cancellation state, so the serve loop cancelling once fans out
            // to every connection. Defaulting here (rather than at each call
            // site) keeps construction ripple-free.
            shutdown: CancellationToken::new(),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            doc_sets: Arc::new(RwLock::new(HashMap::new())),
            connectors: Arc::new(RwLock::new(HashMap::new())),
            pending_confirmations: Arc::new(RwLock::new(HashMap::new())),
            pending_interactions: Arc::new(RwLock::new(HashMap::new())),
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

    /// Install the per-agent behavior-config provider (builder). A host backs
    /// this with its `agents` store so each agent's `instructions` /
    /// `conversation_workflow` drive its conversations. Without it, the runner
    /// falls back to the org-default persona (unchanged behavior).
    #[must_use]
    pub fn with_agent_config(mut self, provider: Arc<dyn AgentConfigResolver>) -> Self {
        self.agent_config = provider;
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

    /// Install a custom Rich Interactions kind catalog (builder). The default
    /// hosts the reference kinds; a host adds its own kinds here.
    #[must_use]
    pub fn with_interactions(mut self, registry: InteractionRegistry) -> Self {
        self.interactions = Arc::new(registry);
        self
    }

    /// Install the end-user OTP identity-verification service (builder). Wires the
    /// `end_user` auth gate to an OTP flow (see [`otp_service`](Self::otp_service));
    /// leaving it unset keeps the fail-closed default (refuse, no OTP offered).
    #[must_use]
    pub fn with_otp_service(mut self, service: Arc<dyn OtpService>) -> Self {
        self.otp_service = Some(service);
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

    /// The conversation-workflow step this session is currently on, read from the
    /// session's `metadata.currentStepId`. `None` = no workflow / fresh start (the
    /// runner then resolves to the workflow's first step).
    #[must_use]
    pub fn session_current_step(&self, session_id: &str) -> Option<String> {
        self.sessions
            .read()
            .ok()?
            .get(session_id)?
            .metadata
            .as_ref()?
            .get("currentStepId")?
            .as_str()
            .map(str::to_string)
    }

    /// Persist the workflow step pointer onto the in-memory session's
    /// `metadata.currentStepId`. Matches the session registry's durability (the
    /// pointer lives as long as the session does, on the pod that owns it). A
    /// `None` step clears the pointer. No-op for an unknown session.
    pub fn set_session_current_step(&self, session_id: &str, step_id: Option<&str>) {
        if let Ok(mut map) = self.sessions.write() {
            if let Some(session) = map.get_mut(session_id) {
                let mut meta = session.metadata.take().unwrap_or_default();
                match step_id {
                    Some(id) => {
                        meta.insert("currentStepId".to_string(), serde_json::Value::from(id));
                    }
                    None => {
                        meta.remove("currentStepId");
                    }
                }
                session.metadata = Some(meta);
            }
        }
    }

    /// The consecutive-non-advancing turn count for this session's current
    /// workflow step, read from `metadata.stepAttempts`. `0` when unset. Feeds the
    /// per-step attempt cap ([`smooth_operator::agent_config::apply_step_cap`]) so a
    /// step the judge never advances can't loop forever. Same durability as the
    /// step pointer (lives with the session, on the pod that owns it).
    #[must_use]
    pub fn session_step_attempts(&self, session_id: &str) -> u32 {
        self.sessions
            .read()
            .ok()
            .and_then(|map| {
                map.get(session_id)?
                    .metadata
                    .as_ref()?
                    .get("stepAttempts")?
                    .as_u64()
            })
            .unwrap_or(0) as u32
    }

    /// Persist the per-step attempt counter onto the in-memory session's
    /// `metadata.stepAttempts`. No-op for an unknown session. Coexists with the
    /// `currentStepId` pointer in the same metadata map.
    pub fn set_session_step_attempts(&self, session_id: &str, attempts: u32) {
        if let Ok(mut map) = self.sessions.write() {
            if let Some(session) = map.get_mut(session_id) {
                let mut meta = session.metadata.take().unwrap_or_default();
                meta.insert("stepAttempts".to_string(), serde_json::Value::from(attempts));
                session.metadata = Some(meta);
            }
        }
    }

    /// Whether this session's caller has completed OTP identity verification,
    /// read from the session's `metadata.otpVerified`. `false` for an unknown or
    /// unverified session. Threaded into the `end_user` auth gate so a verified
    /// session's gated tools run. Same durability as the session registry (lives
    /// as long as the session, on the pod that owns it).
    #[must_use]
    pub fn session_authenticated(&self, session_id: &str) -> bool {
        self.sessions
            .read()
            .ok()
            .and_then(|map| {
                map.get(session_id)?
                    .metadata
                    .as_ref()?
                    .get("otpVerified")?
                    .as_bool()
            })
            .unwrap_or(false)
    }

    /// Mark this session identity-verified (or clear it) by setting
    /// `metadata.otpVerified`. Called after a successful `verify_otp`. No-op for
    /// an unknown session. Coexists with the workflow step pointer (both live in
    /// the session's metadata map).
    pub fn set_session_authenticated(&self, session_id: &str, verified: bool) {
        if let Ok(mut map) = self.sessions.write() {
            if let Some(session) = map.get_mut(session_id) {
                let mut meta = session.metadata.take().unwrap_or_default();
                meta.insert("otpVerified".to_string(), serde_json::Value::from(verified));
                session.metadata = Some(meta);
            }
        }
    }

    /// The caller's OTP contact points for this session, read from the session's
    /// `metadata.contactEmail` / `metadata.contactPhone` (stashed at
    /// create-session time). Empty when the session is unknown or captured no
    /// contact — the server then can't offer OTP. The reference create-session
    /// path captures only an email.
    #[must_use]
    pub fn session_contact(&self, session_id: &str) -> OtpContact {
        let Ok(map) = self.sessions.read() else {
            return OtpContact::default();
        };
        let Some(meta) = map.get(session_id).and_then(|s| s.metadata.as_ref()) else {
            return OtpContact::default();
        };
        OtpContact {
            email: meta
                .get("contactEmail")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            phone: meta
                .get("contactPhone")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        }
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

    /// Register a turn parked on a Rich Interaction for `session_id`. Any prior
    /// pending interaction for the same session is replaced (one outstanding
    /// interaction per session). Called by the runner's interaction bridge when
    /// a raise tool parks.
    pub fn register_interaction(&self, session_id: impl Into<String>, pending: PendingInteraction) {
        if let Ok(mut map) = self.pending_interactions.write() {
            map.insert(session_id.into(), pending);
        }
    }

    /// The pending interaction for `session_id` (id + kind + spec), WITHOUT
    /// consuming the park — an invalid submit must leave the turn parked for a
    /// resubmit. `None` when no interaction is pending. The responder in the
    /// clone is the SAME sender (clones share the channel), but resolution must
    /// go through [`take_interaction`](Self::take_interaction) so duplicates
    /// are no-ops.
    #[must_use]
    pub fn pending_interaction(&self, session_id: &str) -> Option<PendingInteraction> {
        self.pending_interactions
            .read()
            .ok()?
            .get(session_id)
            .cloned()
    }

    /// Take (remove + return) the pending interaction for `session_id`. Taking
    /// it — rather than cloning — guarantees a single submit resolves a single
    /// parked raise, and a duplicate submit is a no-op (`NO_PENDING_INTERACTION`).
    #[must_use]
    pub fn take_interaction(&self, session_id: &str) -> Option<PendingInteraction> {
        self.pending_interactions.write().ok()?.remove(session_id)
    }

    /// Drop any pending interaction registered for `session_id` without
    /// resolving it (parked turn ended — timeout / disconnect). Mirrors
    /// [`clear_confirmation`](Self::clear_confirmation).
    pub fn clear_interaction(&self, session_id: &str) {
        if let Ok(mut map) = self.pending_interactions.write() {
            map.remove(session_id);
        }
    }

    /// The client render capabilities this session declared in `supports` at
    /// `create_conversation_session` (read from the session's
    /// `metadata.supports`). Empty for unknown sessions and text-only channels
    /// — every interaction kind then degrades to its conversational fallback.
    #[must_use]
    pub fn session_capabilities(&self, session_id: &str) -> std::collections::HashSet<String> {
        self.sessions
            .read()
            .ok()
            .and_then(|map| {
                let caps = map
                    .get(session_id)?
                    .metadata
                    .as_ref()?
                    .get("supports")?
                    .as_array()?
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect();
                Some(caps)
            })
            .unwrap_or_default()
    }

    /// Attach validated intake values to the session: metadata `userName` /
    /// `contactEmail` / `contactPhone` — the SAME keys the create-session
    /// (pre-chat) path stashes and the OTP contact seam
    /// ([`session_contact`](Self::session_contact)) reads, so a captured contact
    /// is immediately OTP-verifiable. Only provided fields are written (an
    /// intake that collected just an email never clobbers a known name).
    /// Durable participant/CRM attach is a host concern.
    pub fn attach_session_identity(&self, session_id: &str, values: &IntakeValues) {
        if let Ok(mut map) = self.sessions.write() {
            if let Some(session) = map.get_mut(session_id) {
                let mut meta = session.metadata.take().unwrap_or_default();
                if let Some(name) = &values.name {
                    meta.insert(
                        "userName".to_string(),
                        serde_json::Value::from(name.clone()),
                    );
                }
                if let Some(email) = &values.email {
                    meta.insert(
                        "contactEmail".to_string(),
                        serde_json::Value::from(email.clone()),
                    );
                }
                if let Some(phone) = &values.phone {
                    meta.insert(
                        "contactPhone".to_string(),
                        serde_json::Value::from(phone.clone()),
                    );
                }
                session.metadata = Some(meta);
            }
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
            judge_model: "claude-haiku-4-5".to_string(),
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

    /// Minimal session for the step-tracking tests.
    fn test_session(session_id: &str) -> Session {
        Session {
            session_id: session_id.to_string(),
            conversation_id: "conv".to_string(),
            organization_id: "org".to_string(),
            agent_id: "agent".to_string(),
            agent_name: "Agent".to_string(),
            user_participant_id: "u".to_string(),
            agent_participant_id: "a".to_string(),
            thread_id: "conv".to_string(),
            status: Some(smooth_operator::domain::SessionStatus::Active),
            token_count: Some(0),
            message_count: Some(0),
            metadata: None,
            created_at: None,
            updated_at: None,
            ended_at: None,
            last_activity_at: None,
        }
    }

    #[test]
    fn session_step_tracking_round_trips_and_clears() {
        let state = state_with(config_with_env_key(None));
        state.insert_session(test_session("s1"));

        // Fresh session: no step pointer.
        assert_eq!(state.session_current_step("s1"), None);

        // Set → read back.
        state.set_session_current_step("s1", Some("collect"));
        assert_eq!(
            state.session_current_step("s1"),
            Some("collect".to_string())
        );

        // Overwrite.
        state.set_session_current_step("s1", Some("summary"));
        assert_eq!(
            state.session_current_step("s1"),
            Some("summary".to_string())
        );

        // Clear.
        state.set_session_current_step("s1", None);
        assert_eq!(state.session_current_step("s1"), None);

        // Unknown session is a no-op, not a panic.
        state.set_session_current_step("missing", Some("x"));
        assert_eq!(state.session_current_step("missing"), None);
    }

    #[test]
    fn session_step_is_isolated_per_session() {
        let state = state_with(config_with_env_key(None));
        state.insert_session(test_session("s1"));
        state.insert_session(test_session("s2"));
        state.set_session_current_step("s1", Some("greet"));
        assert_eq!(state.session_current_step("s1"), Some("greet".to_string()));
        assert_eq!(state.session_current_step("s2"), None);
    }

    #[test]
    fn session_authenticated_round_trips_and_defaults_false() {
        let state = state_with(config_with_env_key(None));
        state.insert_session(test_session("s1"));

        // Fresh session: not verified.
        assert!(!state.session_authenticated("s1"));
        // Unknown session: not verified (no panic).
        assert!(!state.session_authenticated("missing"));

        state.set_session_authenticated("s1", true);
        assert!(state.session_authenticated("s1"));

        state.set_session_authenticated("s1", false);
        assert!(!state.session_authenticated("s1"));

        // Verified bit coexists with the workflow step pointer.
        state.set_session_authenticated("s1", true);
        state.set_session_current_step("s1", Some("collect"));
        assert!(state.session_authenticated("s1"));
        assert_eq!(
            state.session_current_step("s1"),
            Some("collect".to_string())
        );
    }

    #[test]
    fn interaction_registry_round_trips_and_peeks_without_consuming() {
        let state = state_with(config_with_env_key(None));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        state.register_interaction(
            "s1",
            PendingInteraction {
                interaction_id: "int-1".into(),
                kind: "identity_intake".into(),
                spec: serde_json::json!({ "fields": [{ "key": "email", "required": true }] }),
                responder: tx,
            },
        );

        // Peek does not consume (an invalid submit must leave the turn parked).
        let p = state.pending_interaction("s1").expect("pending");
        assert_eq!(p.interaction_id, "int-1");
        assert_eq!(p.kind, "identity_intake");
        assert!(state.pending_interaction("s1").is_some());

        // Take consumes; a duplicate submit finds nothing.
        assert!(state.take_interaction("s1").is_some());
        assert!(state.take_interaction("s1").is_none());
        assert!(state.pending_interaction("s1").is_none());

        // clear_interaction drops without resolving.
        let (tx2, _rx2) = tokio::sync::mpsc::unbounded_channel();
        state.register_interaction(
            "s2",
            PendingInteraction {
                interaction_id: "int-2".into(),
                kind: "identity_intake".into(),
                spec: serde_json::Value::Null,
                responder: tx2,
            },
        );
        state.clear_interaction("s2");
        assert!(state.take_interaction("s2").is_none());
    }

    #[test]
    fn attach_session_identity_stamps_contact_keys_without_clobbering() {
        use smooth_operator::identity_intake::IntakeValues;
        let state = state_with(config_with_env_key(None));
        let mut session = test_session("s1");
        let mut meta = std::collections::HashMap::new();
        meta.insert("contactEmail".to_string(), "old@example.com".into());
        meta.insert("userName".to_string(), "Old Name".into());
        session.metadata = Some(meta);
        state.insert_session(session);

        // Only provided fields are written; the known name survives.
        state.attach_session_identity(
            "s1",
            &IntakeValues {
                email: Some("new@example.com".into()),
                phone: Some("+15551234567".into()),
                ..Default::default()
            },
        );
        let contact = state.session_contact("s1");
        assert_eq!(contact.email.as_deref(), Some("new@example.com"));
        assert_eq!(contact.phone.as_deref(), Some("+15551234567"));
        let s = state.get_session("s1").unwrap();
        assert_eq!(
            s.metadata.as_ref().unwrap().get("userName").unwrap(),
            "Old Name"
        );

        // Unknown session is a no-op, not a panic.
        state.attach_session_identity("missing", &IntakeValues::default());
    }

    #[test]
    fn session_capabilities_default_empty_and_read_the_supports_list() {
        let state = state_with(config_with_env_key(None));
        state.insert_session(test_session("s1"));
        assert!(state.session_capabilities("s1").is_empty());
        assert!(state.session_capabilities("missing").is_empty());

        let mut session = test_session("s2");
        let mut meta = std::collections::HashMap::new();
        meta.insert(
            "supports".to_string(),
            serde_json::json!(["identity_form", "date_picker"]),
        );
        session.metadata = Some(meta);
        state.insert_session(session);
        let caps = state.session_capabilities("s2");
        assert!(caps.contains("identity_form"));
        assert!(caps.contains("date_picker"));
        assert!(!caps.contains("file_upload"));
    }

    #[test]
    fn session_contact_reads_stashed_email() {
        let state = state_with(config_with_env_key(None));
        let mut session = test_session("s1");
        let mut meta = std::collections::HashMap::new();
        meta.insert("contactEmail".to_string(), "a@example.com".into());
        session.metadata = Some(meta);
        state.insert_session(session);

        let contact = state.session_contact("s1");
        assert_eq!(contact.email.as_deref(), Some("a@example.com"));
        assert_eq!(contact.phone, None);

        // Unknown / contact-less sessions yield an empty contact.
        assert!(state.session_contact("missing").is_empty());
        state.insert_session(test_session("s2"));
        assert!(state.session_contact("s2").is_empty());
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
