//! Multi-tenancy conformance (feature gap G7) for the Postgres + pgvector
//! `StorageAdapter`, against a real pgvector container via testcontainers.
//!
//! ONE adapter instance serving TWO orgs — the multi-tenant pod shape. The
//! knowledge slice is per-turn tenanted through `knowledge_for_access`, which is
//! what `PgKnowledgeBase::with_access` overrides the org from.
//!
//! The suite body is shared with the in-memory and DynamoDB adapters — see
//! `rust/adapters/multitenancy_suite.rs`.
//!
//! Skips (does not fail) when Docker is unavailable, matching
//! `tests/conformance.rs`.

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

use smooth_operator_adapter_postgres::PostgresAdapter;

#[path = "../../multitenancy_suite.rs"]
mod suite;

/// Spin up a throwaway `pgvector/pgvector:pg16` container. `Ok(None)` ⇒ Docker
/// unavailable ⇒ skip.
async fn start_pgvector() -> anyhow::Result<Option<(ContainerAsync<GenericImage>, String)>> {
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
            eprintln!("SKIP: could not start pgvector container (Docker unavailable?): {e}");
            Ok(None)
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_orgs_are_isolated_on_one_postgres_adapter() -> anyhow::Result<()> {
    let Some((_node, conn_str)) = start_pgvector().await? else {
        return Ok(()); // Docker unavailable — skip, don't fail.
    };
    let store = PostgresAdapter::connect(&conn_str).await?;
    suite::assert_multitenancy(&store, &store, "pg").await;
    Ok(())
}
