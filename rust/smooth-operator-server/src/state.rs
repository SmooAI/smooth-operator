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

use smooth_operator::adapter::{SessionUpdate, StorageAdapter};
use smooth_operator::agent_config::{AgentConfigResolver, StaticAgentConfigResolver};
use smooth_operator::auth::{AuthVerifier, NoAuthVerifier};
use smooth_operator::backplane::{Backplane, InMemoryBackplane};
use smooth_operator::connector_config::{ConnectorConfigStore, InMemoryConnectorConfigStore};
use smooth_operator::domain::{Conversation, Participant, Session};
use smooth_operator::gateway_key::{EnvGatewayKeyResolver, GatewayKeyResolver};
use smooth_operator::identity_intake::IntakeValues;
use smooth_operator::interaction::{InteractionRegistry, InteractionResolution};
use smooth_operator::otp::{OtpContact, OtpService};
use smooth_operator::settings::{InMemorySettingsStore, SettingsStore};
use smooth_operator::tool_provider::ToolProvider;
use smooth_operator::widget_auth::{PermissiveWidgetAuth, WidgetAuthProvider};
use tokio_util::sync::CancellationToken;

use smooth_operator_core::executor::AgentExecutor;
use smooth_operator_core::llm_provider::LlmProvider;
use smooth_operator_core::tool::ToolHook;
use smooth_operator_ingestion::indexing::{InMemoryIndexingStore, IndexingStore};

use crate::config::ServerConfig;

/// A create-session's storage writes, held back until the conversation earns a
/// row (SMOODEV-3057).
///
/// Opening a widget used to write a `conversations` row (plus both participants
/// and a session) before the visitor had typed anything. In a 30-day production
/// sample **44 of 117** web conversations carried zero messages — bare opens
/// occupying an inbox row — and because a web create feeds a fresh UUID as the
/// conversation's `idempotency_key`, the unique index's `ON CONFLICT DO NOTHING`
/// could never collapse a double-connect the way it does for sms/slack/discord.
///
/// Parking the writes here fixes both: an abandoned open leaves nothing behind,
/// and a double-connect is harmless because neither connection has written.
///
/// Each field is cleared as its write lands, so a retry after a transient
/// storage failure resumes rather than re-inserting an already-taken id.
pub struct PendingSession {
    /// The session this create would have produced — the key it is parked under.
    pub session_id: String,
    /// The connection that opened it; its close discards the parked writes.
    pub conn_id: String,
    /// Written first: every other row FKs it, and a host adapter's participant
    /// hook may read its `metadata_json`.
    pub conversation: Option<Conversation>,
    /// The `user` participant — the write a host adapter hooks for CRM capture.
    pub user_participant: Option<Participant>,
    /// The `ai_agent` participant.
    pub agent_participant: Option<Participant>,
    /// Written last, as in the eager path.
    pub session: Option<Session>,
}

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
    /// **Host approver seam** (th-be3f55). Channels from a host-installed hook
    /// whose `Ask` verdicts should park the turn and route through the chat HITL
    /// — the same bridge `SMOOTH_AGENT_CONFIRM_TOOLS` uses, but driven by the
    /// host's own classification instead of tool-name patterns.
    ///
    /// Big Smooth's auto-mode gate is the motivating case: the core
    /// `PermissionHook` could already classify a call as `Ask`, but with no
    /// approver wired it failed closed, forcing the daemon into `Bypass` where
    /// nothing is ever asked. `None` (the default) changes nothing.
    pub host_approver: Option<crate::runner::HostApprover>,
    /// **Host tool-injection seam.** When `Some`, the runner asks this provider
    /// for EXTRA tools and merges them into every turn's `ToolRegistry`
    /// alongside the built-ins. Defaults to `None` (built-ins only); a host
    /// installs one via [`with_tools`](Self::with_tools) to contribute its own
    /// per-org tool catalog without forking the runner.
    pub tool_provider: Option<Arc<dyn ToolProvider>>,
    /// **Host tool-hook seam.** Engine [`ToolHook`]s the host installs on every
    /// turn's `ToolRegistry` (registered before the per-agent auth gate and
    /// confirmation hooks, so a host permission/surveillance hook gets first say).
    /// Empty by default (no extra hooks); a host installs some via
    /// [`with_tool_hooks`](Self::with_tool_hooks). This is the seam Big Smooth uses
    /// to inject its auto-mode permission gate + narc judge into every turn.
    pub tool_hooks: Vec<Arc<dyn ToolHook>>,
    /// **Host turn-executor seam.** The [`AgentExecutor`] every turn runs on.
    /// `None` (the default) is the in-process executor — a verbatim delegation to
    /// `Agent::run_with_channel` — so a host that never installs one is
    /// byte-for-byte unchanged.
    ///
    /// Two things arrive through here. A durable backend (ADR-030) is the one the
    /// trait was written for. The other is a **decorator**: an executor that
    /// delegates to `InProcessExecutor` and then inspects or edits the returned
    /// `Conversation` before the runner reads its final assistant message. That is
    /// the only host-side seam on the emitted reply, and it is what a post-response
    /// guard needs — the conversation carries the turn's tool calls, so a host can
    /// strip a claim the tools never backed (pearl th-39999c).
    ///
    /// Note the boundary: tokens the turn streamed have already left over the
    /// events channel by the time the conversation is returned, so an edit here
    /// changes the persisted message and the `eventual_response`, not what already
    /// streamed. A decorator that needs the stream too can pass its own channel
    /// down and forward.
    pub executor: Option<Arc<dyn AgentExecutor>>,
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
    /// **Skill-resolution seam** for `send_message.skill`. When `Some`, a turn
    /// naming a skill has its body resolved here and composed into the turn's
    /// system prompt, so the wire carries the *intent* ("use skill X") rather
    /// than the client prepending the skill's prose to the message. `None` (the
    /// default) ⇒ any `skill` field is a clean `SKILL_NOT_FOUND` error and no
    /// turn runs. Installed via [`with_skill_resolver`](Self::with_skill_resolver);
    /// [`build_state`](crate::server::build_state) installs the filesystem
    /// default when `SMOOTH_SKILLS_DIR` is set.
    pub skill_resolver: Option<Arc<dyn crate::skills::SkillResolver>>,
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
    /// **Held-back create-session writes** (SMOODEV-3057): `sessionId` →
    /// [`PendingSession`]. An anonymous widget open that carries no visitor
    /// identity is parked here instead of being written, and lands in storage on
    /// its first message ([`materialize_session`](Self::materialize_session)) or
    /// is dropped when the connection closes
    /// ([`discard_pending_sessions_for_conn`](Self::discard_pending_sessions_for_conn)).
    pending_sessions: Arc<RwLock<HashMap<String, PendingSession>>>,
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
    /// **One-shot pre-approved confirmations** (th-db0816): `sessionId` → tool
    /// name whose next confirmation-gated call this pod may approve WITHOUT
    /// parking. Granted only by `handle_confirm_tool_action` when it resolves a
    /// DURABLE pending confirmation (the parked turn lived on another pod, or
    /// died with one) and spawns a continuation turn to carry out the approved
    /// action. Consumed by the very next turn's `ConfirmationConfig`; never
    /// readable from the wire, so a client cannot smuggle a bypass in a frame.
    pre_approved_confirmations: Arc<RwLock<HashMap<String, String>>>,
    /// **Rich Interactions pending parks**: `sessionId` → the parked turn's
    /// interaction (id + kind + spec) and [`InteractionResolution`] sender. When an
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
    /// Id of this interaction instance (minted by the raise tool); the submit
    /// must echo it so a stale submit can never resolve a newer park.
    pub interaction_id: String,
    /// The interaction kind (routes to its validator).
    pub kind: String,
    /// The kind-specific spec the raise carried (drives validation).
    pub spec: serde_json::Value,
    /// Resumes the parked raise tool.
    pub responder: UnboundedSender<InteractionResolution>,
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
            host_approver: None,
            storage,
            config: Arc::new(config),
            auth: Arc::new(NoAuthVerifier::default()),
            indexing: Arc::new(InMemoryIndexingStore::new()),
            connector_configs: Arc::new(InMemoryConnectorConfigStore::new()),
            settings: Arc::new(InMemorySettingsStore::new()),
            tool_provider: None,
            tool_hooks: Vec::new(),
            executor: None,
            widget_auth: Arc::new(PermissiveWidgetAuth),
            agent_config: Arc::new(StaticAgentConfigResolver::default()),
            backplane: Arc::new(InMemoryBackplane::new()),
            chat_provider: None,
            gateway_key_resolver,
            otp_service: None,
            skill_resolver: None,
            interactions: Arc::new(InteractionRegistry::default()),
            // A fresh, never-cancelled token: every clone of this state shares
            // its cancellation state, so the serve loop cancelling once fans out
            // to every connection. Defaulting here (rather than at each call
            // site) keeps construction ripple-free.
            shutdown: CancellationToken::new(),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            pending_sessions: Arc::new(RwLock::new(HashMap::new())),
            doc_sets: Arc::new(RwLock::new(HashMap::new())),
            connectors: Arc::new(RwLock::new(HashMap::new())),
            pending_confirmations: Arc::new(RwLock::new(HashMap::new())),
            pre_approved_confirmations: Arc::new(RwLock::new(HashMap::new())),
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

    /// Install host [`ToolHook`]s (builder) applied to every turn's tool registry,
    /// before the per-agent auth gate and confirmation hooks — so a host
    /// permission/surveillance hook gets first say on every call. Empty (the
    /// default) ⇒ no extra hooks. Big Smooth uses this to inject its auto-mode
    /// permission gate + narc judge.
    #[must_use]
    pub fn with_tool_hooks(mut self, hooks: Vec<Arc<dyn ToolHook>>) -> Self {
        self.tool_hooks = hooks;
        self
    }

    /// Install the [`AgentExecutor`] every turn runs on (builder). `None` — the
    /// default — is the in-process executor, so omitting this is unchanged
    /// behavior. See [`executor`](Self::executor) for the two things that arrive
    /// through this seam (a durable backend, or a decorator that guards the reply).
    #[must_use]
    pub fn with_executor(mut self, executor: Arc<dyn AgentExecutor>) -> Self {
        self.executor = Some(executor);
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

    /// Install the skill resolver (builder) backing `send_message.skill`. A host
    /// (Big Smooth) installs one over its own skill discovery; without it, a
    /// `skill` field is rejected with `SKILL_NOT_FOUND` rather than silently
    /// running an unskilled turn.
    #[must_use]
    pub fn with_skill_resolver(mut self, resolver: Arc<dyn crate::skills::SkillResolver>) -> Self {
        self.skill_resolver = Some(resolver);
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

    /// Park a create-session's storage writes until the conversation earns them
    /// (SMOODEV-3057). See [`PendingSession`].
    pub fn defer_session(&self, pending: PendingSession) {
        if let Ok(mut map) = self.pending_sessions.write() {
            map.insert(pending.session_id.clone(), pending);
        }
    }

    /// True while `session_id`'s create is still parked (nothing written yet).
    /// Test/introspection helper.
    #[must_use]
    pub fn is_session_deferred(&self, session_id: &str) -> bool {
        self.pending_sessions
            .read()
            .is_ok_and(|map| map.contains_key(session_id))
    }

    /// Flush a parked create to storage, in the SAME order the eager path used:
    /// conversation → user participant → agent participant → session. That order
    /// is load-bearing twice over — every child row FKs the conversation, and a
    /// host adapter may hook the `user` participant write to capture the visitor
    /// into its CRM, reading phone/consent off the conversation's `metadata_json`
    /// (SmooAI's `chat-storage::crm_capture` does exactly that).
    ///
    /// A no-op for a session that was never deferred, so callers need no branch.
    ///
    /// Each step clears itself from the parked record only once it has actually
    /// landed, so a retry after a transient failure resumes where it stopped
    /// rather than re-inserting a row whose primary key is already taken.
    pub async fn materialize_session(&self, session_id: &str) -> anyhow::Result<()> {
        let Some(mut pending) = ({
            let mut map = self
                .pending_sessions
                .write()
                .map_err(|_| anyhow::anyhow!("pending-session registry poisoned"))?;
            map.remove(session_id)
        }) else {
            return Ok(());
        };
        let result = self.flush_pending(&mut pending).await;
        if result.is_err() {
            // Put it back so the visitor's retry finishes the job. Dropping it
            // here would leave a live session whose rows can never be written.
            self.defer_session(pending);
        }
        result
    }

    /// Land the parked create that owns `conversation_id`, if one is parked here.
    ///
    /// A reconnect names a `conversationId`; if that conversation's create is
    /// still parked (the visitor opened the widget, the socket blipped, and they
    /// had not yet typed) the resume would otherwise find nothing and mint a
    /// fresh conversation — losing the durable `supports` record a reconnect that
    /// omits `supports` inherits, and the conversation id itself. Landing it
    /// first keeps the resume path behaving exactly as it did when every create
    /// wrote immediately.
    ///
    /// Writing rows for a caller-named id before the ownership check leaks
    /// nothing: it persists what this pod already holds, reads nothing back to
    /// the caller, and the resume is still gated by `may_read_conversation`.
    pub async fn materialize_conversation(&self, conversation_id: &str) -> anyhow::Result<()> {
        let Some(session_id) = ({
            let map = self
                .pending_sessions
                .read()
                .map_err(|_| anyhow::anyhow!("pending-session registry poisoned"))?;
            map.values()
                .find(|pending| {
                    pending
                        .session
                        .as_ref()
                        .is_some_and(|s| s.conversation_id == conversation_id)
                })
                .map(|pending| pending.session_id.clone())
        }) else {
            return Ok(());
        };
        self.materialize_session(&session_id).await
    }

    async fn flush_pending(&self, pending: &mut PendingSession) -> anyhow::Result<()> {
        if let Some(conversation) = pending.conversation.clone() {
            self.storage.create_conversation(conversation).await?;
            pending.conversation = None;
        }
        if let Some(participant) = pending.user_participant.clone() {
            self.storage.add_participant(participant).await?;
            pending.user_participant = None;
        }
        if let Some(participant) = pending.agent_participant.clone() {
            self.storage.add_participant(participant).await?;
            pending.agent_participant = None;
        }
        if let Some(session) = pending.session.clone() {
            self.storage.create_session(session).await?;
            pending.session = None;
        }
        Ok(())
    }

    /// Drop every parked create belonging to a closed connection — an open that
    /// never sent a message never earns its rows, and without this the pending
    /// map would grow for the pod's whole lifetime.
    pub fn discard_pending_sessions_for_conn(&self, conn_id: &str) {
        if let Ok(mut map) = self.pending_sessions.write() {
            map.retain(|_, pending| pending.conn_id != conn_id);
        }
    }

    /// Look up a session in the LOCAL registry only.
    ///
    /// Prefer [`load_session`](Self::load_session) on any request path: this map
    /// is a per-pod cache, so a hit means "some connection on THIS pod touched
    /// this session", not "this session exists".
    #[must_use]
    pub fn get_session(&self, session_id: &str) -> Option<Session> {
        self.sessions.read().ok()?.get(session_id).cloned()
    }

    /// Look up a session, falling back to durable storage and priming the local
    /// registry on a miss (th-ca579c).
    ///
    /// The local map is per-pod. The widget's returning-visitor resume POSTs
    /// `/internal/resume-by-fingerprint`, which primes the session on whichever
    /// pod served that HTTP request, and then opens a WebSocket — which the load
    /// balancer sends to an arbitrary pod. With >1 replica and no session
    /// affinity the two rarely match, so the first `send_message` hit an empty
    /// map and the visitor was told `session '<id>' not found` in the chat
    /// bubble. At 2 replicas that is roughly half of returning visitors; the HPA
    /// goes to 6.
    ///
    /// Storage is the source of truth, so any pod can serve any session. Priming
    /// afterwards keeps the existing SYNCHRONOUS readers
    /// ([`session_authenticated`](Self::session_authenticated),
    /// [`session_contact`](Self::session_contact),
    /// [`session_supports`](Self::session_supports)) working untouched: every
    /// frame that needs them passes the ownership check first, and that check is
    /// what calls this.
    /// `Ok(None)` is an existence claim — this session does not exist. `Err` is
    /// NOT: a storage failure means the question could not be answered, and the
    /// caller must surface it as retryable rather than telling a human their
    /// session is gone. The two used to collapse into `None`, so a Postgres blip
    /// put `session '<id>' not found` in a live visitor's chat bubble.
    pub async fn load_session(&self, session_id: &str) -> anyhow::Result<Option<Session>> {
        if let Some(session) = self.get_session(session_id) {
            return Ok(Some(session));
        }
        match self.storage.get_session(session_id).await {
            Ok(Some(session)) => {
                // th-694c22: this is the cross-pod resume in action — a session
                // another pod created, served here via storage. The one line
                // that tells an incident responder routing worked as designed.
                tracing::info!(
                    session_id,
                    "session primed from storage (not in local registry)"
                );
                self.insert_session(session.clone());
                Ok(Some(session))
            }
            Ok(None) => Ok(None),
            Err(e) => {
                tracing::warn!(error = %e, session_id, "session lookup failed against storage");
                Err(e)
            }
        }
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

    // The per-pod `stepAttempts` accessors that used to live here are gone
    // (th-fc07ac). th-c12df5 moved the workflow step pointer AND its attempt count
    // onto durable conversation metadata precisely because this per-pod session map
    // resets on reconnect/pod hop, which froze the workflow on its first step. The
    // readers/writers are `load_workflow_step` / `persist_workflow_step` in
    // `handler.rs`; nothing should read the attempt count off a session again.

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
    /// Mark this session identity-verified (or clear it), in the local registry
    /// AND in durable storage (th-ca579c).
    ///
    /// Persisting matters more here than for anything else in the metadata blob:
    /// this is the OTP gate's answer. Local-only, a caller who proved their
    /// identity on one pod was silently unverified on the next frame if the load
    /// balancer moved them, and unverified again after any pod roll — while the
    /// gate itself was working exactly as designed.
    ///
    /// The write is local-first so the current turn sees it immediately, then
    /// through to storage. A storage failure is logged, not raised: the turn in
    /// flight has already verified the human, and failing it here would refuse
    /// service to someone who just proved who they are. The cost is that the
    /// verification may not survive a pod hop, which is exactly the pre-fix
    /// behaviour — a degradation, not a regression.
    pub async fn set_session_authenticated(&self, session_id: &str, verified: bool) {
        let updated = {
            let Ok(mut map) = self.sessions.write() else {
                return;
            };
            let Some(session) = map.get_mut(session_id) else {
                return;
            };
            let mut meta = session.metadata.take().unwrap_or_default();
            meta.insert("otpVerified".to_string(), serde_json::Value::from(verified));
            session.metadata = Some(meta.clone());
            meta
        };
        if let Err(e) = self
            .storage
            .update_session(
                session_id,
                SessionUpdate {
                    metadata: Some(updated),
                    ..Default::default()
                },
            )
            .await
        {
            tracing::warn!(
                error = %e,
                session_id,
                "persisting otpVerified failed — verification will not survive a pod hop"
            );
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

    /// Grant a ONE-SHOT pre-approval for `tool` on `session_id`'s next turn
    /// (th-db0816). Set only by `handle_confirm_tool_action` when it resolves a
    /// durable pending confirmation and spawns the continuation turn; consumed
    /// by [`take_pre_approval`](Self::take_pre_approval) when that turn's
    /// `ConfirmationConfig` is built, so the re-issued tool call executes
    /// without parking a second time.
    pub fn grant_pre_approval(&self, session_id: impl Into<String>, tool: impl Into<String>) {
        if let Ok(mut map) = self.pre_approved_confirmations.write() {
            map.insert(session_id.into(), tool.into());
        }
    }

    /// Take (remove + return) the one-shot pre-approved tool name for
    /// `session_id`, if any. Taking — not reading — is what makes the grant
    /// one-shot: only the single turn spawned by the resolving
    /// `confirm_tool_action` ever sees it.
    #[must_use]
    pub fn take_pre_approval(&self, session_id: &str) -> Option<String> {
        self.pre_approved_confirmations
            .write()
            .ok()?
            .remove(session_id)
    }

    /// Set or clear the DURABLE record of a parked write-tool confirmation on
    /// the session's `metadata.pendingConfirmation` (th-db0816), local-first
    /// then written through to storage — the same shape as
    /// [`set_session_authenticated`](Self::set_session_authenticated).
    ///
    /// The in-process park (the [`HumanResponse`] sender in
    /// `pending_confirmations`) is a channel into a turn running on THIS pod, so
    /// it cannot survive a pod hop or a roll — which is exactly when a visitor's
    /// refresh reconnects them elsewhere and their pending write-confirmation
    /// used to evaporate. This record is the half that survives: tool name,
    /// arguments and prompt, enough for `handle_confirm_tool_action` on any pod
    /// to carry out the verdict with a continuation turn.
    ///
    /// A storage failure is logged, not raised, when SETTING (the in-process
    /// park still works exactly as before — a degradation, not a regression).
    /// Clearing returns the storage error to the caller, because the one caller
    /// that must not proceed past a failed clear (the continuation path, which
    /// is about to execute a write tool) needs to fail closed.
    pub async fn set_pending_confirmation(
        &self,
        session_id: &str,
        record: Option<serde_json::Value>,
    ) -> anyhow::Result<()> {
        self.set_session_meta_key(session_id, "pendingConfirmation", record)
            .await
    }

    /// Set or clear the DURABLE record of a parked Rich Interaction on the
    /// session's `metadata.pendingInteraction` — the interaction sibling of
    /// [`set_pending_confirmation`](Self::set_pending_confirmation) (th-db0816).
    /// The record carries the interaction id, kind and spec, so a
    /// `submit_interaction` landing on a pod without the live park can still
    /// validate and resolve it. Unlike confirmations it carries no TTL: the
    /// resolution's host effect (identity attach) is idempotent and
    /// non-destructive, so a late submit is a feature, not a hazard.
    pub async fn set_pending_interaction(
        &self,
        session_id: &str,
        record: Option<serde_json::Value>,
    ) -> anyhow::Result<()> {
        self.set_session_meta_key(session_id, "pendingInteraction", record)
            .await
    }

    /// Shared write path for the durable park records: set/remove one metadata
    /// key locally, then write the whole map through to storage (the same
    /// local-first shape as [`set_session_authenticated`](Self::set_session_authenticated)).
    /// A storage failure is returned so a caller about to act on a retired
    /// record can fail closed.
    async fn set_session_meta_key(
        &self,
        session_id: &str,
        key: &str,
        value: Option<serde_json::Value>,
    ) -> anyhow::Result<()> {
        let updated = {
            let Ok(mut map) = self.sessions.write() else {
                return Ok(());
            };
            let Some(session) = map.get_mut(session_id) else {
                return Ok(());
            };
            let mut meta = session.metadata.take().unwrap_or_default();
            match &value {
                Some(v) => {
                    meta.insert(key.to_string(), v.clone());
                }
                None => {
                    meta.remove(key);
                }
            }
            session.metadata = Some(meta.clone());
            meta
        };
        if let Err(e) = self
            .storage
            .update_session(
                session_id,
                SessionUpdate {
                    metadata: Some(updated),
                    ..Default::default()
                },
            )
            .await
        {
            tracing::warn!(
                error = %e,
                session_id,
                key,
                "persisting session metadata key failed — it will not survive a pod hop"
            );
            return Err(e);
        }
        Ok(())
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
        let updated = {
            let Ok(mut map) = self.sessions.write() else {
                return;
            };
            let Some(session) = map.get_mut(session_id) else {
                return;
            };
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
            session.metadata = Some(meta.clone());
            meta
        };
        // Write through to storage (th-db0816): a captured contact used to live
        // only in this pod's map, so a pod roll forgot who the visitor was even
        // though they had just told us. Spawned (this is a sync seam) and
        // best-effort — the in-process attach above already served the turn.
        // Guarded so the sync unit tests, which run without a runtime, still
        // exercise the local half.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let storage = Arc::clone(&self.storage);
            let sid = session_id.to_string();
            handle.spawn(async move {
                if let Err(e) = storage
                    .update_session(
                        &sid,
                        SessionUpdate {
                            metadata: Some(updated),
                            ..Default::default()
                        },
                    )
                    .await
                {
                    tracing::warn!(error = %e, session_id = %sid, "persisting attached identity failed — it will not survive a pod hop");
                }
            });
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
            agent_id: Some("agent".to_string()),
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

    /// th-ca579c — the production bug, as a test.
    ///
    /// Two pods share storage and share NOTHING else. A session created through
    /// pod A's registry must be servable by pod B, which has never seen it. This
    /// is exactly what the smoo.ai widget hit: `/internal/resume-by-fingerprint`
    /// primed pod A, the WebSocket landed on pod B, and the visitor was told
    /// `session '<id>' not found` in the chat bubble.
    #[tokio::test]
    async fn a_second_pod_can_serve_a_session_it_never_saw() {
        let storage = Arc::new(InMemoryStorageAdapter::new());
        let pod_a = AppState::new(storage.clone(), config_with_env_key(None));
        let pod_b = AppState::new(storage.clone(), config_with_env_key(None));

        // Pod A creates it. Only storage is shared.
        let session = storage
            .create_session(test_session("s-shared"))
            .await
            .expect("create");
        pod_a.insert_session(session);

        // The local-only accessor still shows the split — that is the premise,
        // not an incidental detail. If this ever passes, the two pods are
        // sharing memory and the rest of this test proves nothing.
        assert!(
            pod_b.get_session("s-shared").is_none(),
            "premise broken: pod B must not have it locally"
        );

        let loaded = pod_b.load_session("s-shared").await.expect("storage ok");
        assert!(
            loaded.is_some(),
            "pod B must hydrate the session from storage"
        );
        assert_eq!(loaded.unwrap().session_id, "s-shared");

        // ...and it primes, so the synchronous readers on this pod work for the
        // rest of the frame.
        assert!(
            pod_b.get_session("s-shared").is_some(),
            "hydration must prime the local registry"
        );
    }

    /// A session that genuinely does not exist still resolves to `None` — the
    /// hydration path must not invent one.
    #[tokio::test]
    async fn hydration_does_not_conjure_an_unknown_session() {
        let state = state_with(config_with_env_key(None));
        assert!(state
            .load_session("s-nope")
            .await
            .expect("storage ok")
            .is_none());
    }

    /// th-ca579c — identity verification survives a pod hop.
    ///
    /// `otpVerified` used to live only in the serving pod's map, so a caller who
    /// completed OTP on pod A was silently unverified on pod B and after every
    /// roll. The gate was working; the answer was not durable.
    #[tokio::test]
    async fn otp_verification_survives_a_pod_hop() {
        let storage = Arc::new(InMemoryStorageAdapter::new());
        let pod_a = AppState::new(storage.clone(), config_with_env_key(None));
        let pod_b = AppState::new(storage.clone(), config_with_env_key(None));

        let session = storage
            .create_session(test_session("s-otp"))
            .await
            .expect("create");
        pod_a.insert_session(session);

        pod_a.set_session_authenticated("s-otp", true).await;
        assert!(pod_a.session_authenticated("s-otp"));

        // Pod B hydrates fresh from storage and must agree.
        assert!(
            pod_b
                .load_session("s-otp")
                .await
                .expect("storage ok")
                .is_some(),
            "pod B must see the session"
        );
        assert!(
            pod_b.session_authenticated("s-otp"),
            "a verified caller must stay verified when the load balancer moves them"
        );
    }

    #[tokio::test]
    async fn session_authenticated_round_trips_and_defaults_false() {
        let state = state_with(config_with_env_key(None));
        state.insert_session(test_session("s1"));

        // Fresh session: not verified.
        assert!(!state.session_authenticated("s1"));
        // Unknown session: not verified (no panic).
        assert!(!state.session_authenticated("missing"));

        state.set_session_authenticated("s1", true).await;
        assert!(state.session_authenticated("s1"));

        state.set_session_authenticated("s1", false).await;
        assert!(!state.session_authenticated("s1"));

        // Verified bit coexists with the workflow step pointer.
        state.set_session_authenticated("s1", true).await;
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
