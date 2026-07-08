//! Postgres-backed [`AgentConfigResolver`] over the monorepo `agents` table.
//!
//! The reference server points its Postgres storage backend at the same database
//! the SmooAI monorepo owns (the schema in [`crate::schema`] mirrors that shape),
//! so the `agents` row for a connection's `agent_id` is reachable on the adapter's
//! existing pool — no second connection, no HTTP hop. This provider reads the
//! per-agent behavior knobs (`instructions`, `personality`, `greeting`,
//! `conversation_workflow`, `tool_config`) so the runner can honor them.
//!
//! **Failure-tolerant by construction**: a non-UUID `agent_id`, an absent row, a
//! missing `agents` table (a standalone deploy whose DB has only the operator's
//! own tables), or a malformed jsonb value all resolve to `None` / an empty
//! config — the turn falls back to the org-default persona rather than failing.

use async_trait::async_trait;
use deadpool_postgres::Pool;
use tracing::debug;

use smooth_operator::agent_config::{AgentBehaviorConfig, AgentConfigResolver};

/// Postgres-backed [`AgentConfigResolver`] over the `agents` table.
#[derive(Clone)]
pub struct PgAgentConfigResolver {
    pool: Pool,
}

impl PgAgentConfigResolver {
    /// Build over the adapter's async pool.
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Query the `agents` row, mapping any failure to `None` (see module docs).
    async fn fetch(&self, agent_id: &str) -> Option<AgentBehaviorConfig> {
        // `agents.id` is a uuid; a widget/session `agentId` that isn't a valid
        // uuid can't match a row (and would make Postgres error on the cast), so
        // short-circuit to None.
        let id = match uuid::Uuid::parse_str(agent_id) {
            Ok(id) => id,
            Err(_) => {
                debug!(agent_id, "agent_id is not a uuid; no per-agent config");
                return None;
            }
        };

        let client = match self.pool.get().await {
            Ok(c) => c,
            Err(e) => {
                debug!(error = %e, "agent config: pool.get failed; falling back to org default");
                return None;
            }
        };

        let row = match client
            .query_opt(
                "SELECT instructions, personality, greeting, conversation_workflow, tool_config, extension_config, visibility, model, max_iterations \
                 FROM agents WHERE id = $1",
                &[&id],
            )
            .await
        {
            Ok(row) => row?,
            Err(e) => {
                // Missing table (standalone deploy) or any query error: degrade.
                debug!(error = %e, agent_id, "agent config query failed; falling back to org default");
                return None;
            }
        };

        // Column reads are `Option` so a NULL / unexpected type never panics.
        let instructions: Option<serde_json::Value> = row.try_get("instructions").ok().flatten();
        let personality: Option<serde_json::Value> = row.try_get("personality").ok().flatten();
        let greeting: Option<String> = row.try_get("greeting").ok().flatten();
        let workflow: Option<serde_json::Value> =
            row.try_get("conversation_workflow").ok().flatten();
        let tool_config: Option<serde_json::Value> = row.try_get("tool_config").ok().flatten();
        // `try_get(...).ok().flatten()` also degrades a standalone deploy whose
        // `agents` table predates the `extension_config` column to `None` — the
        // column-absent read errors, `.ok()` swallows it, and we get "no per-agent
        // extension list" (backward-compatible; see `enabled_extensions` handling).
        let extension_config: Option<serde_json::Value> =
            row.try_get("extension_config").ok().flatten();
        let visibility: Option<String> = row.try_get("visibility").ok().flatten();
        // Per-agent model + loop-cap overrides (SMOODEV-2172). Same forward-compat
        // read as `extension_config`: a standalone deploy whose `agents` table
        // predates these columns errors on the read, `.ok()` swallows it, and we
        // fall back to the global `SMOOTH_AGENT_MODEL` / `SMOOTH_AGENT_MAX_ITERATIONS`
        // defaults. `max_iterations` is read as `i32` (drizzle `integer` = int4) and
        // widened; `from_row_values` clamps it to `1..=64`.
        let model: Option<String> = row.try_get("model").ok().flatten();
        let max_iterations: Option<i64> = row
            .try_get::<_, Option<i32>>("max_iterations")
            .ok()
            .flatten()
            .map(i64::from);

        let config = AgentBehaviorConfig::from_row_values(
            instructions,
            personality,
            greeting,
            workflow,
            tool_config,
            extension_config,
            visibility,
            model,
            max_iterations,
        );
        // Discriminator is ROW EXISTENCE, not `is_empty()`: `query_opt`'s `None`
        // (handled above via `row?`) is the only "no per-agent config" case. A row
        // that exists but carries all-NULL config columns still yields `Some(empty)`
        // — this is the SEP fail-closed contract (SMOODEV-2259): a resolved agent
        // that lists no `enabled_extensions` must load ZERO extensions, so it must
        // be distinguishable from "no resolver / agent unknown" (which fails open to
        // the server allowlist). Returning `Some(empty)` is behavior-preserving for
        // the persona/tool/workflow seams — each already treats an empty config the
        // same as `None` (`system_prompt()`/`enabled_tool_ids()` return `None`,
        // workflow is absent), so only the extension gate observes the difference.
        Some(config)
    }
}

#[async_trait]
impl AgentConfigResolver for PgAgentConfigResolver {
    async fn resolve(&self, agent_id: &str) -> Option<AgentBehaviorConfig> {
        self.fetch(agent_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Behavior against a live Postgres is covered by the parity/integration
    // suites; here we assert the credential-free invariants that must hold with
    // no database reachable.

    #[tokio::test]
    async fn non_uuid_agent_id_is_none_without_touching_db() {
        // A pool pointed at an unreachable host proves the uuid guard returns
        // BEFORE any `pool.get()` — the bogus host is never dialed.
        let mut cfg = deadpool_postgres::Config::new();
        cfg.host = Some("127.0.0.1".to_string());
        cfg.port = Some(1); // nothing listens here
        cfg.dbname = Some("nope".to_string());
        cfg.user = Some("nobody".to_string());
        let pool = cfg
            .create_pool(
                Some(deadpool_postgres::Runtime::Tokio1),
                tokio_postgres::NoTls,
            )
            .expect("build pool");
        let provider = PgAgentConfigResolver::new(pool);
        assert!(provider.resolve("not-a-uuid").await.is_none());
    }
}
