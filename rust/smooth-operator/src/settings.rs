//! Per-org agent settings storage (Phase 12, increment 3).
//!
//! The management console reads + writes an org's **agent settings** — the model,
//! the system prompt, and the default tool set — through the admin write API. A
//! [`SettingsStore`] persists one [`AgentSettings`] per org; an unset org reads
//! back [`AgentSettings::defaults`] rather than `None`, so the console always has
//! a populated form to edit.
//!
//! These are *configuration*, not secrets — no `auth_ref` model applies here.
//!
//! ## Persistence
//!
//! Ships with an [`InMemorySettingsStore`]. The persistent follow-up is a
//! Postgres/DynamoDB `agent_settings` table keyed on `org_id` —
//! [`put`](SettingsStore::put) is an upsert, [`get`](SettingsStore::get) a single
//! `SELECT` falling back to defaults. Only the trait + in-memory impl are built
//! here.

use std::collections::HashMap;
use std::sync::RwLock;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The default model an org uses until settings are saved.
pub const DEFAULT_MODEL: &str = "gpt-4o-mini";

/// The default system prompt for a fresh org.
pub const DEFAULT_SYSTEM_PROMPT: &str =
    "You are a helpful support assistant. Answer using the provided knowledge; \
     cite sources and say you don't know when the knowledge doesn't cover the question.";

/// Per-org agent configuration: model, system prompt, and default tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSettings {
    /// The owning organization.
    pub org_id: String,
    /// The LLM model id the agent runs on.
    pub model: String,
    /// The agent's system prompt.
    pub system_prompt: String,
    /// Tool names enabled by default for this org's agent.
    pub default_tools: Vec<String>,
    /// When the settings were last written.
    pub updated_at: DateTime<Utc>,
}

impl AgentSettings {
    /// The defaults an org starts from before any settings are saved.
    #[must_use]
    pub fn defaults(org_id: impl Into<String>) -> Self {
        Self {
            org_id: org_id.into(),
            model: DEFAULT_MODEL.to_string(),
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_string(),
            default_tools: Vec::new(),
            updated_at: Utc::now(),
        }
    }
}

/// Storage seam for per-org [`AgentSettings`]. Org-scoped: a caller only ever
/// reads / writes its own org's settings.
pub trait SettingsStore: Send + Sync {
    /// The org's settings, or [`AgentSettings::defaults`] when unset.
    fn get(&self, org_id: &str) -> AgentSettings;

    /// Insert or replace the org's settings.
    fn put(&self, settings: AgentSettings);
}

/// In-memory [`SettingsStore`] keyed on `org_id`.
#[derive(Default)]
pub struct InMemorySettingsStore {
    rows: RwLock<HashMap<String, AgentSettings>>,
}

impl InMemorySettingsStore {
    /// A fresh, empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl SettingsStore for InMemorySettingsStore {
    fn get(&self, org_id: &str) -> AgentSettings {
        self.rows
            .read()
            .ok()
            .and_then(|rows| rows.get(org_id).cloned())
            .unwrap_or_else(|| AgentSettings::defaults(org_id))
    }

    fn put(&self, settings: AgentSettings) {
        if let Ok(mut rows) = self.rows.write() {
            rows.insert(settings.org_id.clone(), settings);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_unset_returns_defaults_scoped_to_org() {
        let store = InMemorySettingsStore::new();
        let s = store.get("org-x");
        assert_eq!(s.org_id, "org-x");
        assert_eq!(s.model, DEFAULT_MODEL);
        assert!(!s.system_prompt.is_empty());
        assert!(s.default_tools.is_empty());
    }

    #[test]
    fn put_then_get_reflects_change_and_is_org_scoped() {
        let store = InMemorySettingsStore::new();
        store.put(AgentSettings {
            org_id: "org-a".into(),
            model: "claude-x".into(),
            system_prompt: "be terse".into(),
            default_tools: vec!["knowledge_search".into(), "fetch_url".into()],
            updated_at: Utc::now(),
        });

        let a = store.get("org-a");
        assert_eq!(a.model, "claude-x");
        assert_eq!(a.system_prompt, "be terse");
        assert_eq!(a.default_tools, vec!["knowledge_search", "fetch_url"]);

        // A different org still sees defaults.
        assert_eq!(store.get("org-b").model, DEFAULT_MODEL);
    }

    #[test]
    fn put_replaces_existing() {
        let store = InMemorySettingsStore::new();
        store.put(AgentSettings {
            org_id: "o".into(),
            model: "m1".into(),
            system_prompt: "p".into(),
            default_tools: vec![],
            updated_at: Utc::now(),
        });
        store.put(AgentSettings {
            org_id: "o".into(),
            model: "m2".into(),
            system_prompt: "p".into(),
            default_tools: vec![],
            updated_at: Utc::now(),
        });
        assert_eq!(store.get("o").model, "m2");
    }
}
