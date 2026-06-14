//! Conformance tests for the pgvector-backed [`PgMemory`] (parity gap Phase 3 /
//! SMOODEV-1470), run against a real pgvector container via testcontainers.
//!
//! Asserts the persistent + semantic memory contract:
//!
//! - **store → recall** returns the semantically-closest entry (deterministic
//!   embedder, no network),
//! - **org/user scoping** isolates namespaces — org A can't recall org B's
//!   memory, and a user-scoped handle can't recall another user's (or org-wide)
//!   memory,
//! - **forget** removes an entry, and
//! - **recall on empty** returns empty.
//!
//! The container requires a running Docker daemon. If Docker is unavailable the
//! test **skips** (prints a notice and returns Ok) rather than failing, so CI
//! without Docker stays green — exactly like `conformance.rs`.

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

use smooth_operator_core::{Memory, MemoryEntry, MemoryType};

use smooth_operator_adapter_postgres::PostgresAdapter;

/// Spin up a throwaway `pgvector/pgvector:pg16` container. Returns `Ok(None)` if
/// Docker is unavailable so the caller can skip rather than fail.
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

/// `PgMemory` drives async work from a sync trait via `spawn` + a blocking OS
/// thread, so it must be exercised off the async worker threads (just like the
/// checkpoint store in `conformance.rs`). Each memory operation runs inside
/// `spawn_blocking`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pgvector_memory_persists_and_recalls_semantically() -> anyhow::Result<()> {
    let Some((_node, conn_str)) = start_pgvector().await? else {
        return Ok(()); // Docker unavailable — skip, don't fail.
    };

    let store = PostgresAdapter::connect(&conn_str).await?;

    // Two org/user namespaces to prove isolation.
    let mem_a = store.memory("org-A", Some("user-1".to_string()));
    let mem_b = store.memory("org-B", Some("user-9".to_string()));
    let mem_a_orgwide = store.memory("org-A", None);

    // --- recall on empty returns empty ---
    let empty = tokio::task::spawn_blocking({
        let mem = mem_a.clone();
        move || mem.recall("anything at all", 5)
    })
    .await??;
    assert!(empty.is_empty(), "recall on empty namespace must be empty");

    // --- store several distinctive memories into org-A/user-1 ---
    let pizza_id = {
        let entry = MemoryEntry::new(
            "The user loves pepperoni pizza and Italian food.",
            MemoryType::User,
        )
        .with_metadata("topic", "food");
        let id = entry.id.clone();
        tokio::task::spawn_blocking({
            let mem = mem_a.clone();
            move || mem.store(entry)
        })
        .await??;
        id
    };
    {
        let entry = MemoryEntry::new(
            "The user works as a structural civil engineer on bridges.",
            MemoryType::User,
        );
        tokio::task::spawn_blocking({
            let mem = mem_a.clone();
            move || mem.store(entry)
        })
        .await??;
    }
    {
        let entry = MemoryEntry::new(
            "Photosynthesis converts sunlight into chemical energy in plants.",
            MemoryType::Reference,
        );
        tokio::task::spawn_blocking({
            let mem = mem_a.clone();
            move || mem.store(entry)
        })
        .await??;
    }

    // --- store a memory into a DIFFERENT org so we can prove isolation ---
    {
        let entry = MemoryEntry::new(
            "The user loves pepperoni pizza and Italian food.",
            MemoryType::User,
        );
        tokio::task::spawn_blocking({
            let mem = mem_b.clone();
            move || mem.store(entry)
        })
        .await??;
    }

    // --- store → recall returns the semantically-closest entry ---
    // A food-flavored query must surface the pizza memory first, not the
    // engineering or photosynthesis entries.
    let recalled = tokio::task::spawn_blocking({
        let mem = mem_a.clone();
        move || mem.recall("what kind of food does the user enjoy eating", 3)
    })
    .await??;
    assert!(!recalled.is_empty(), "recall returned no results");
    assert!(
        recalled[0].content.contains("pizza"),
        "semantically-closest entry must rank first (got {:?})",
        recalled.iter().map(|m| &m.content).collect::<Vec<_>>()
    );
    // relevance is the cosine similarity (1 - distance), sorted descending.
    assert!(
        recalled[0].relevance >= recalled[recalled.len() - 1].relevance,
        "results must be ordered by descending relevance"
    );
    // Metadata + type round-trip.
    assert_eq!(recalled[0].memory_type, MemoryType::User);
    assert_eq!(
        recalled[0].metadata.get("topic").map(String::as_str),
        Some("food")
    );

    // --- org/user scoping isolates namespaces ---
    // org-B has exactly one (pizza) memory; org-A's three must NOT appear there,
    // and vice-versa. A high limit forces the namespace filter to do the work.
    let from_b = tokio::task::spawn_blocking({
        let mem = mem_b.clone();
        move || mem.recall("structural civil engineer bridges", 50)
    })
    .await??;
    assert_eq!(
        from_b.len(),
        1,
        "org-B namespace must only see its own single memory, got {}",
        from_b.len()
    );
    assert!(from_b[0].content.contains("pizza"));

    // The org-wide (user_id = None) handle for org-A is a DISTINCT namespace from
    // org-A/user-1: it must not see user-1's rows.
    let from_orgwide = tokio::task::spawn_blocking({
        let mem = mem_a_orgwide.clone();
        move || mem.recall("pizza food engineer", 50)
    })
    .await??;
    assert!(
        from_orgwide.is_empty(),
        "org-A/org-wide namespace must not see org-A/user-1 rows, got {}",
        from_orgwide.len()
    );

    // --- forget removes the entry ---
    tokio::task::spawn_blocking({
        let mem = mem_a.clone();
        let id = pizza_id.clone();
        move || mem.forget(&id)
    })
    .await??;
    let after_forget = tokio::task::spawn_blocking({
        let mem = mem_a.clone();
        move || mem.recall("what kind of food does the user enjoy eating", 50)
    })
    .await??;
    assert!(
        after_forget.iter().all(|m| m.id != pizza_id),
        "forgotten entry must not be recalled"
    );

    // --- forget is namespace-scoped: org-B's identical pizza memory survives ---
    let b_survives = tokio::task::spawn_blocking({
        let mem = mem_b.clone();
        move || mem.recall("pizza", 50)
    })
    .await??;
    assert_eq!(
        b_survives.len(),
        1,
        "forget in org-A must not touch org-B's memory"
    );

    println!("POSTGRES MEMORY CONFORMANCE: store + semantic recall + org/user scoping + forget + empty-recall passed against pgvector/pgvector:pg16");

    drop(store);
    Ok(())
}
