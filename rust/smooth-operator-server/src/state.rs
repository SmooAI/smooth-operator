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
use smooth_operator::connector_config::{ConnectorConfigStore, InMemoryConnectorConfigStore};
use smooth_operator::domain::Session;
use smooth_operator::settings::{InMemorySettingsStore, SettingsStore};

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
    /// Session registry: `sessionId` → session blob. Shared across connections.
    sessions: Arc<RwLock<HashMap<String, Session>>>,
    /// Document-set registry: set name → document count. The in-memory knowledge
    /// backend drops document metadata on ingest, so the admin API reads
    /// document-set membership from this side registry (populated as docs are
    /// seeded / ingested through the server). Connector names are also recorded
    /// for `GET /admin/indexing/runs`.
    doc_sets: Arc<RwLock<HashMap<String, usize>>>,
    /// Connector names whose indexing runs should be listed (the
    /// `InMemoryIndexingStore` lists per-connector). Recorded as runs are added.
    connectors: Arc<RwLock<Vec<String>>>,
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
            sessions: Arc::new(RwLock::new(HashMap::new())),
            doc_sets: Arc::new(RwLock::new(HashMap::new())),
            connectors: Arc::new(RwLock::new(Vec::new())),
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

    /// Record that a document was added to a named document set (increments its
    /// count). Used by seeding + the ingest path so `GET /admin/document-sets`
    /// can report set names + counts despite the in-memory backend dropping
    /// document metadata.
    pub fn record_document_set(&self, set: impl Into<String>) {
        if let Ok(mut map) = self.doc_sets.write() {
            *map.entry(set.into()).or_insert(0) += 1;
        }
    }

    /// Snapshot the document-set registry as `(name, count)` pairs, sorted by
    /// name for a stable response.
    #[must_use]
    pub fn document_sets(&self) -> Vec<(String, usize)> {
        let Ok(map) = self.doc_sets.read() else {
            return Vec::new();
        };
        let mut out: Vec<(String, usize)> = map.iter().map(|(k, v)| (k.clone(), *v)).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Record a connector whose indexing runs should be listed (idempotent).
    pub fn record_connector(&self, name: impl Into<String>) {
        let name = name.into();
        if let Ok(mut v) = self.connectors.write() {
            if !v.iter().any(|c| c == &name) {
                v.push(name);
            }
        }
    }

    /// Snapshot the recorded connector names (sorted, stable).
    #[must_use]
    pub fn connectors(&self) -> Vec<String> {
        let Ok(v) = self.connectors.read() else {
            return Vec::new();
        };
        let mut out = v.clone();
        out.sort();
        out
    }
}
