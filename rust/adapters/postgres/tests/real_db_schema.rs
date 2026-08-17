//! The acceptance test for th-5a5181: this adapter's schema must APPLY against the real
//! smooai monorepo database.
//!
//! It did not. `schema.rs` declared a `seq BIGSERIAL` on `conversation_messages` that the
//! monorepo has never had, so `CREATE INDEX ... (conversation_id, seq)` aborted init with
//! `column "seq" does not exist` — every server applying that schema failed at boot against
//! the real database, which is what made dogfooding-on-the-real-DB impossible.
//!
//! Gated like the repo's other live tests: needs a real database, so it is `#[ignore]`d and
//! reads its connection string from `SMOOTH_AGENT_REAL_DB_URL`. Run it against local Supabase:
//!
//! ```text
//! SMOOTH_AGENT_REAL_DB_URL='postgresql://postgres:postgres@127.0.0.1:54332/postgres' \
//!   cargo test -p smooai-smooth-operator-adapter-postgres --all-features \
//!   --test real_db_schema -- --ignored --nocapture
//! ```
//!
//! This asserts the schema APPLIES, not that the adapter can read those rows — it cannot yet.
//! Ids are `uuid` there and `TEXT` here, and `direction` is a real Postgres enum; see the
//! remaining-divergences list in `schema.rs`.
//!
//! # Currently RED, one layer deeper than it was
//!
//! The `seq` failure this test was written for is fixed. Run today against local Supabase it
//! gets past the OLTP tables and fails on the knowledge table instead:
//!
//! ```text
//! Error: applying knowledge_vectors schema
//! Caused by: ERROR: column "embedding" does not exist
//! ```
//!
//! Same class of bug, different subsystem: the real `knowledge_vectors` is a different table
//! than this adapter declares — `embedding` is `embedding_v2` there, `document_id` is
//! `knowledge_document_id`, `metadata` is `meta_data`, `source` does not exist at all, and it
//! carries `name` / `filters` / `usage` / `content_hash` / `content_id` / `file_id` /
//! `owner_user_id` besides. The `CREATE INDEX ... USING hnsw (embedding ...)` is the statement
//! that dies, exactly as `CREATE INDEX ... (seq)` did.
//!
//! Reconciling it means ~42 column references across the dense∪sparse retrieval path in
//! `knowledge.rs`, so it is deliberately NOT in the th-5a5181 change that fixed the
//! conversation tables — it needs its own decision, not a drive-by.
//!
//! ⚠️ While you are in there: `schema.rs` runs
//! `ALTER TABLE knowledge_vectors ADD COLUMN IF NOT EXISTS acl JSONB`, which SUCCEEDS against
//! the real database. Booting this adapter at a real smooai database therefore MUTATES a
//! production table — an unreviewed schema change performed by a server at startup. That
//! wants removing or gating regardless of which way the rest is reconciled.

use smooth_operator_adapter_postgres::PostgresAdapter;

#[tokio::test]
#[ignore = "needs a real smooai database: set SMOOTH_AGENT_REAL_DB_URL"]
async fn schema_applies_against_the_real_monorepo_database() -> anyhow::Result<()> {
    let Ok(conn_str) = std::env::var("SMOOTH_AGENT_REAL_DB_URL") else {
        eprintln!("SKIP: set SMOOTH_AGENT_REAL_DB_URL to run this");
        return Ok(());
    };

    // Connecting applies the schema. Before th-5a5181 this returned the `seq` error.
    let adapter = PostgresAdapter::connect(&conn_str).await?;

    // Applying twice must also work — init is idempotent, and a server restart re-runs it.
    let _again = PostgresAdapter::connect(&conn_str).await?;

    drop(adapter);
    Ok(())
}
