//! Connector configuration storage (Phase 12, increment 3).
//!
//! The management console configures **connectors** — a `github`/`web`/`file`
//! source the indexing loop pulls documents from — through the admin write API.
//! A [`ConnectorConfig`] is the persisted, org-scoped description of one such
//! source; the admin API CRUDs them and, on demand, builds a live
//! `smooth_operator_ingestion::Connector` from one to trigger an indexing run.
//!
//! ## The `auth_ref` secret model — never store the secret
//!
//! A connector's `config` payload (a free-form [`serde_json::Value`]) may carry
//! an **`auth_ref`** — the *name* of an environment variable / secret (e.g.
//! `"GITHUB_TOKEN"`), **never the token itself**. The actual credential is
//! resolved from the environment (or `@smooai/config` when deployed) at *index
//! time*, never persisted in the store and never returned in an API response.
//! This keeps the config store free of secret material: a leaked store row, log
//! line, or API response exposes only a *reference*, not a credential.
//!
//! ## Persistence
//!
//! Ships with an [`InMemoryConnectorConfigStore`]. The persistent follow-up is a
//! Postgres/DynamoDB `connector_configs` table keyed on `(org_id, id)` —
//! [`upsert`](ConnectorConfigStore::upsert) is an INSERT … ON CONFLICT, `list`
//! is `SELECT … WHERE org_id = $1`, `delete` is a scoped `DELETE`. Only the trait
//! and in-memory impl are built here; the persistent adapters follow as siblings
//! of the existing conversation/checkpoint adapters (see `docs/ADMIN-API.md`).

use std::collections::HashMap;
use std::sync::RwLock;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The kind of source a [`ConnectorConfig`] describes. Mirrors the built-in
/// `smooth_operator_ingestion` connectors (`github` / `web` / `file`); an
/// unknown wire value is rejected at the API boundary with a clean 400.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectorKind {
    /// A GitHub repository (prose/code/issues) — `GithubConnector`.
    Github,
    /// A single public web URL — `WebConnector`.
    Web,
    /// A local file tree — `FileConnector`.
    File,
}

impl ConnectorKind {
    /// Parse a kind from a wire string (case-insensitive).
    ///
    /// # Errors
    /// Returns `Err(value)` (the offending input) when not a known kind, so the
    /// caller can build a precise 400 message.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "github" => Ok(Self::Github),
            "web" => Ok(Self::Web),
            "file" => Ok(Self::File),
            other => Err(other.to_string()),
        }
    }

    /// The wire/string form of this kind.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Web => "web",
            Self::File => "file",
        }
    }
}

/// A persisted, org-scoped connector configuration.
///
/// The `config` payload is connector-kind-specific and free-form so a new
/// connector kind needs no schema migration. For `github` it carries
/// `owner` / `repo` and optionally `ref` / `include` / `visibility`; for `web`
/// a `url`; for `file` a `path`. The optional **`auth_ref`** names the secret to
/// resolve at index time — the secret value is **never** stored here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorConfig {
    /// Stable id (uuid v4), unique within the org.
    pub id: String,
    /// The owning organization — every store operation is scoped to this.
    pub org_id: String,
    /// Human-readable name for the connector.
    pub name: String,
    /// The source kind.
    pub kind: ConnectorKind,
    /// Kind-specific configuration (owner/repo/url/path/include/ref/…). May carry
    /// an `auth_ref` naming a secret; never the secret itself.
    pub config: Value,
    /// Whether the connector is active (a disabled connector is configured but
    /// won't be auto-indexed by a scheduler; manual `/index` still works).
    pub enabled: bool,
    /// When the config row was created.
    pub created_at: DateTime<Utc>,
    /// When the config row was last updated.
    pub updated_at: DateTime<Utc>,
}

impl ConnectorConfig {
    /// The `auth_ref` (secret name) from the `config` payload, if present and a
    /// non-empty string. This is the *name* to resolve from env/config at index
    /// time — never a token value.
    #[must_use]
    pub fn auth_ref(&self) -> Option<&str> {
        self.config
            .get("auth_ref")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }
}

/// Storage seam for [`ConnectorConfig`]s. Every method is org-scoped so a
/// caller can only ever see / mutate its own org's connectors.
pub trait ConnectorConfigStore: Send + Sync {
    /// All connector configs for `org_id`, sorted by `name` (stable).
    fn list(&self, org_id: &str) -> Vec<ConnectorConfig>;

    /// One connector config by `(org_id, id)`, or `None` if absent / in another
    /// org (cross-org reads return `None`, never another org's row).
    fn get(&self, org_id: &str, id: &str) -> Option<ConnectorConfig>;

    /// Insert or update a connector config (keyed on `(org_id, id)`).
    fn upsert(&self, config: ConnectorConfig);

    /// Delete a connector config by `(org_id, id)`. Returns whether a row was
    /// removed (so the API can 404 a delete of an absent / cross-org id).
    fn delete(&self, org_id: &str, id: &str) -> bool;
}

/// In-memory [`ConnectorConfigStore`] keyed on `(org_id, id)`.
#[derive(Default)]
pub struct InMemoryConnectorConfigStore {
    /// `(org_id, id)` → config.
    rows: RwLock<HashMap<(String, String), ConnectorConfig>>,
}

impl InMemoryConnectorConfigStore {
    /// A fresh, empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl ConnectorConfigStore for InMemoryConnectorConfigStore {
    fn list(&self, org_id: &str) -> Vec<ConnectorConfig> {
        let Ok(rows) = self.rows.read() else {
            return Vec::new();
        };
        let mut out: Vec<ConnectorConfig> = rows
            .values()
            .filter(|c| c.org_id == org_id)
            .cloned()
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        out
    }

    fn get(&self, org_id: &str, id: &str) -> Option<ConnectorConfig> {
        let rows = self.rows.read().ok()?;
        rows.get(&(org_id.to_string(), id.to_string())).cloned()
    }

    fn upsert(&self, config: ConnectorConfig) {
        if let Ok(mut rows) = self.rows.write() {
            rows.insert((config.org_id.clone(), config.id.clone()), config);
        }
    }

    fn delete(&self, org_id: &str, id: &str) -> bool {
        if let Ok(mut rows) = self.rows.write() {
            rows.remove(&(org_id.to_string(), id.to_string())).is_some()
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(org: &str, id: &str, name: &str, kind: ConnectorKind, config: Value) -> ConnectorConfig {
        let now = Utc::now();
        ConnectorConfig {
            id: id.into(),
            org_id: org.into(),
            name: name.into(),
            kind,
            config,
            enabled: true,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn kind_parse_roundtrips_and_rejects_unknown() {
        assert_eq!(
            ConnectorKind::parse("github").unwrap(),
            ConnectorKind::Github
        );
        assert_eq!(ConnectorKind::parse("  WEB ").unwrap(), ConnectorKind::Web);
        assert_eq!(ConnectorKind::parse("File").unwrap(), ConnectorKind::File);
        assert_eq!(ConnectorKind::Github.as_str(), "github");
        assert_eq!(ConnectorKind::parse("slack").unwrap_err(), "slack");
    }

    #[test]
    fn upsert_list_get_are_org_scoped() {
        let store = InMemoryConnectorConfigStore::new();
        store.upsert(cfg(
            "org-a",
            "1",
            "beta",
            ConnectorKind::Web,
            json!({"url": "https://b"}),
        ));
        store.upsert(cfg(
            "org-a",
            "2",
            "alpha",
            ConnectorKind::Web,
            json!({"url": "https://a"}),
        ));
        store.upsert(cfg(
            "org-b",
            "3",
            "gamma",
            ConnectorKind::Web,
            json!({"url": "https://g"}),
        ));

        // org-a sees only its two, sorted by name.
        let a = store.list("org-a");
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].name, "alpha");
        assert_eq!(a[1].name, "beta");

        // org-b sees only its one.
        assert_eq!(store.list("org-b").len(), 1);

        // Cross-org get returns None (org-b can't read org-a's id "1").
        assert!(store.get("org-b", "1").is_none());
        assert!(store.get("org-a", "1").is_some());
    }

    #[test]
    fn upsert_updates_in_place() {
        let store = InMemoryConnectorConfigStore::new();
        store.upsert(cfg(
            "o",
            "1",
            "name-1",
            ConnectorKind::Web,
            json!({"url": "https://1"}),
        ));
        store.upsert(cfg(
            "o",
            "1",
            "name-2",
            ConnectorKind::Web,
            json!({"url": "https://2"}),
        ));
        let got = store.get("o", "1").unwrap();
        assert_eq!(got.name, "name-2");
        assert_eq!(store.list("o").len(), 1, "upsert replaces, not appends");
    }

    #[test]
    fn delete_is_org_scoped_and_reports_removal() {
        let store = InMemoryConnectorConfigStore::new();
        store.upsert(cfg(
            "o",
            "1",
            "n",
            ConnectorKind::File,
            json!({"path": "/d"}),
        ));
        // Cross-org delete is a no-op.
        assert!(!store.delete("other", "1"));
        assert!(store.get("o", "1").is_some());
        // Scoped delete removes + reports true; a second delete reports false.
        assert!(store.delete("o", "1"));
        assert!(!store.delete("o", "1"));
        assert!(store.get("o", "1").is_none());
    }

    #[test]
    fn auth_ref_reads_secret_name_not_value() {
        let with = cfg(
            "o",
            "1",
            "n",
            ConnectorKind::Github,
            json!({"owner": "o", "repo": "r", "auth_ref": "GITHUB_TOKEN"}),
        );
        assert_eq!(with.auth_ref(), Some("GITHUB_TOKEN"));

        // Absent / blank auth_ref ⇒ None.
        let without = cfg(
            "o",
            "2",
            "n",
            ConnectorKind::Github,
            json!({"owner": "o", "repo": "r"}),
        );
        assert_eq!(without.auth_ref(), None);
        let blank = cfg(
            "o",
            "3",
            "n",
            ConnectorKind::Github,
            json!({"auth_ref": "  "}),
        );
        assert_eq!(blank.auth_ref(), None);
    }
}
