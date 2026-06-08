//! Conformance tests for the persistent **admin stores** (Phase 12 follow-up),
//! run against a real Postgres container via testcontainers.
//!
//! Mirrors the in-memory store semantics in `smooth-operator/src/connector_config.rs`,
//! `smooth-operator/src/settings.rs`, and `ingestion/src/indexing.rs` — proving
//! the Postgres impls are drop-in durable replacements:
//!
//! - **connector configs**: CRUD + org-isolation (org A's connector invisible to
//!   org B), `list` sorted by name, `upsert` updates in place, cross-org delete
//!   is a no-op.
//! - **settings**: `put` → `get` round-trip, `get` of an unset org returns
//!   defaults, org-scoped.
//! - **indexing runs**: `record_run` → `list_runs` (oldest-first) → upsert by id;
//!   `latest_cursor` is the max cursor over **succeeded** runs only (a failed run
//!   never advances it).
//!
//! The container requires a running Docker daemon. If Docker is unavailable the
//! test **skips** (prints a notice, returns Ok) rather than failing.

use chrono::{TimeZone, Utc};
use serde_json::json;

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

use smooth_operator::connector_config::{ConnectorConfig, ConnectorConfigStore, ConnectorKind};
use smooth_operator::settings::{AgentSettings, SettingsStore, DEFAULT_MODEL};
use smooth_operator_adapter_postgres::PostgresAdapter;
use smooth_operator_ingestion::indexing::{IndexingRun, IndexingRunStatus, IndexingStore};
use smooth_operator_ingestion::Timestamp;

/// Spin up a throwaway `postgres:16` container (the admin stores need no
/// pgvector). Returns `Ok(None)` if Docker is unavailable so the caller skips.
async fn start_postgres() -> anyhow::Result<Option<(ContainerAsync<GenericImage>, String)>> {
    // pgvector image works equally well; use it to stay aligned with the storage
    // conformance test (PostgresAdapter::connect creates the vector extension).
    let image = GenericImage::new("pgvector/pgvector", "pg16")
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_exposed_port(5432.tcp())
        .with_env_var("POSTGRES_USER", "postgres")
        .with_env_var("POSTGRES_PASSWORD", "postgres")
        .with_env_var("POSTGRES_DB", "postgres");

    match image.start().await {
        Ok(node) => {
            let host = node.get_host().await?;
            let port = node.get_host_port_ipv4(5432).await?;
            let conn_str =
                format!("host={host} port={port} user=postgres password=postgres dbname=postgres");
            Ok(Some((node, conn_str)))
        }
        Err(e) => {
            eprintln!("SKIP: could not start postgres container (Docker unavailable?): {e}");
            Ok(None)
        }
    }
}

fn connector(
    org: &str,
    id: &str,
    name: &str,
    kind: ConnectorKind,
    config: serde_json::Value,
) -> ConnectorConfig {
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

fn ts(y: i32, mo: u32, d: u32) -> Timestamp {
    Utc.with_ymd_and_hms(y, mo, d, 0, 0, 0).unwrap()
}

fn run(name: &str, status: IndexingRunStatus, cursor: Option<Timestamp>) -> IndexingRun {
    IndexingRun {
        id: uuid::Uuid::new_v4().to_string(),
        connector_name: name.to_string(),
        status,
        started_at: Utc::now(),
        finished_at: Some(Utc::now()),
        documents_seen: 0,
        chunks_indexed: 0,
        documents_skipped: 0,
        cursor,
        error: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admin_stores_round_trip_through_postgres() -> anyhow::Result<()> {
    let Some((_node, conn_str)) = start_postgres().await? else {
        return Ok(()); // Docker unavailable — skip, don't fail.
    };

    let store = PostgresAdapter::connect(&conn_str).await?;

    // The store traits are synchronous and bridge to the async pool internally;
    // drive them off the async worker threads exactly as the admin API would.
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        // ---- connector configs: CRUD + org isolation + list ordering ----
        let connectors = store.connector_config_store();

        connectors.upsert(connector(
            "org-a",
            "1",
            "beta",
            ConnectorKind::Web,
            json!({"url": "https://b"}),
        ));
        connectors.upsert(connector(
            "org-a",
            "2",
            "alpha",
            ConnectorKind::Github,
            json!({"owner": "o", "repo": "r", "auth_ref": "GITHUB_TOKEN"}),
        ));
        connectors.upsert(connector(
            "org-b",
            "3",
            "gamma",
            ConnectorKind::File,
            json!({"path": "/d"}),
        ));

        // org-a sees only its two, sorted by name (alpha before beta).
        let a = connectors.list("org-a");
        assert_eq!(a.len(), 2, "org-a sees exactly its two connectors");
        assert_eq!(a[0].name, "alpha");
        assert_eq!(a[1].name, "beta");
        // kind + jsonb config round-trip intact.
        assert_eq!(a[0].kind, ConnectorKind::Github);
        assert_eq!(a[0].auth_ref(), Some("GITHUB_TOKEN"));

        // org-b sees only its one — org A's connectors are invisible to org B.
        let b = connectors.list("org-b");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].id, "3");

        // Cross-org get returns None (org-b can't read org-a's id "1").
        assert!(connectors.get("org-b", "1").is_none());
        assert!(connectors.get("org-a", "1").is_some());

        // upsert updates in place (keyed on (org_id, id)).
        connectors.upsert(connector(
            "org-a",
            "1",
            "beta-renamed",
            ConnectorKind::Web,
            json!({"url": "https://b2"}),
        ));
        assert_eq!(connectors.list("org-a").len(), 2, "upsert replaces, not appends");
        assert_eq!(connectors.get("org-a", "1").unwrap().name, "beta-renamed");

        // Cross-org delete is a no-op; scoped delete removes + reports true.
        assert!(!connectors.delete("org-b", "1"), "cross-org delete is a no-op");
        assert!(connectors.get("org-a", "1").is_some());
        assert!(connectors.delete("org-a", "1"));
        assert!(!connectors.delete("org-a", "1"), "second delete reports false");
        assert!(connectors.get("org-a", "1").is_none());

        // ---- settings: put -> get + defaults-when-absent + org scope ----
        let settings = store.settings_store();

        // Unset org reads back defaults (not None).
        let unset = settings.get("org-x");
        assert_eq!(unset.org_id, "org-x");
        assert_eq!(unset.model, DEFAULT_MODEL);
        assert!(unset.default_tools.is_empty());

        settings.put(AgentSettings {
            org_id: "org-a".into(),
            model: "claude-x".into(),
            system_prompt: "be terse".into(),
            default_tools: vec!["knowledge_search".into(), "fetch_url".into()],
            updated_at: Utc::now(),
        });
        let got = settings.get("org-a");
        assert_eq!(got.model, "claude-x");
        assert_eq!(got.system_prompt, "be terse");
        assert_eq!(got.default_tools, vec!["knowledge_search", "fetch_url"]);

        // A different org still sees defaults (org-scoped).
        assert_eq!(settings.get("org-b").model, DEFAULT_MODEL);

        // put replaces existing.
        settings.put(AgentSettings {
            org_id: "org-a".into(),
            model: "claude-y".into(),
            system_prompt: "be verbose".into(),
            default_tools: vec![],
            updated_at: Utc::now(),
        });
        assert_eq!(settings.get("org-a").model, "claude-y");
        assert!(settings.get("org-a").default_tools.is_empty());

        // ---- indexing: record_run -> list_runs(asc) -> latest_cursor ----
        let indexing = store.indexing_store();

        indexing.record_run(&run("c", IndexingRunStatus::Succeeded, Some(ts(2026, 1, 1))));
        indexing.record_run(&run("c", IndexingRunStatus::Succeeded, Some(ts(2026, 1, 2))));
        indexing.record_run(&run("other", IndexingRunStatus::Succeeded, Some(ts(2026, 1, 9))));

        let runs = indexing.list_runs("c");
        assert_eq!(runs.len(), 2, "only this connector's runs");
        assert_eq!(runs[0].cursor, Some(ts(2026, 1, 1)), "oldest-first ordering");
        assert_eq!(runs[1].cursor, Some(ts(2026, 1, 2)));
        assert_eq!(indexing.list_runs("other").len(), 1);
        assert!(indexing.list_runs("missing").is_empty());

        // latest_cursor = max over SUCCEEDED runs; a later FAILED run with a
        // higher (nonsense) cursor must NOT advance it.
        indexing.record_run(&run("c", IndexingRunStatus::Failed, Some(ts(2026, 12, 31))));
        assert_eq!(
            indexing.latest_cursor("c"),
            Some(ts(2026, 1, 2)),
            "failed run does not advance the cursor"
        );
        assert_eq!(indexing.latest_cursor("never-seen"), None);

        // record_run upserts by id: a Running row promotes to a terminal state.
        let mut r = run("up", IndexingRunStatus::Running, None);
        indexing.record_run(&r);
        assert_eq!(indexing.list_runs("up").len(), 1);
        r.status = IndexingRunStatus::Succeeded;
        r.cursor = Some(ts(2026, 1, 5));
        r.documents_seen = 7;
        r.chunks_indexed = 12;
        indexing.record_run(&r);
        let up = indexing.list_runs("up");
        assert_eq!(up.len(), 1, "upsert by id replaces, not appends");
        assert_eq!(up[0].status, IndexingRunStatus::Succeeded);
        assert_eq!(up[0].documents_seen, 7);
        assert_eq!(up[0].chunks_indexed, 12);
        assert_eq!(indexing.latest_cursor("up"), Some(ts(2026, 1, 5)));

        println!("POSTGRES ADMIN CONFORMANCE: connector CRUD + org-isolation, settings defaults/put-get, indexing record/list/cursor (succeeded-only) passed against pgvector/pgvector:pg16");

        // Drop the adapter inside the blocking task so its sync checkpoint pool
        // disposes off the async worker threads (see PostgresAdapter::drop).
        drop(store);
        Ok(())
    })
    .await??;

    Ok(())
}
