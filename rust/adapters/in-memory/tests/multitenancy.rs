//! Multi-tenancy conformance (feature gap G7) for the in-memory `StorageAdapter`.
//!
//! ONE adapter instance serving TWO orgs — the shared-pod shape, and the harshest
//! variant: every slice (tables, knowledge side table, checkpoints) is literally
//! the same object for both tenants, so any missing org partition shows up
//! immediately rather than being hidden behind two separate instances.
//!
//! The suite body is shared with the Postgres and DynamoDB adapters — see
//! `rust/adapters/multitenancy_suite.rs`.

#[path = "../../multitenancy_suite.rs"]
mod suite;

#[tokio::test]
async fn two_orgs_are_isolated_on_one_in_memory_adapter() {
    let store = smooth_operator_adapter_memory::InMemoryStorageAdapter::new();
    suite::assert_multitenancy(&store, &store, "mem").await;
}
