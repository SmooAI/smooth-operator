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

use smooth_operator::adapter::StorageAdapter;
use smooth_operator::auth::{AuthVerifier, NoAuthVerifier};
use smooth_operator::backplane::{Backplane, InMemoryBackplane};
use smooth_operator::connector_config::{ConnectorConfigStore, InMemoryConnectorConfigStore};
use smooth_operator::domain::Session;
use smooth_operator::settings::{InMemorySettingsStore, SettingsStore};
use smooth_operator::widget_auth::{PermissiveWidgetAuth, WidgetAuthProvider};

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
        Self {
            storage,
            config: Arc::new(config),
            auth: Arc::new(NoAuthVerifier::default()),
            indexing: Arc::new(InMemoryIndexingStore::new()),
            connector_configs: Arc::new(InMemoryConnectorConfigStore::new()),
            settings: Arc::new(InMemorySettingsStore::new()),
            widget_auth: Arc::new(PermissiveWidgetAuth),
            backplane: Arc::new(InMemoryBackplane::new()),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            doc_sets: Arc::new(RwLock::new(HashMap::new())),
            connectors: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Install the configured auth verifier (builder).
    #[must_use]
    pub fn with_auth(mut self, auth: Arc<dyn AuthVerifier>) -> Self {
        self.auth = auth;
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
}
