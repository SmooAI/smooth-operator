//! Multi-tenancy conformance (feature gap G7) for the DynamoDB single-table
//! `StorageAdapter`, against a real `amazon/dynamodb-local` container.
//!
//! ONE adapter instance serving TWO orgs — the multi-tenant pod shape. The
//! knowledge slice partitions per tenant: ingest writes the document's own
//! `org_id`, and a query reads the requester's org partition.
//!
//! The suite body is shared with the in-memory and Postgres adapters — see
//! `rust/adapters/multitenancy_suite.rs`. Skip policy: `tests/common/mod.rs`.

mod common;

#[path = "../../multitenancy_suite.rs"]
mod suite;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_orgs_are_isolated_on_one_dynamodb_adapter() -> anyhow::Result<()> {
    let Some((_node, store)) = common::start().await? else {
        return Ok(()); // Docker unavailable or port unreachable — skip, don't fail.
    };
    suite::assert_multitenancy(&store, &store, "ddb").await;
    Ok(())
}
