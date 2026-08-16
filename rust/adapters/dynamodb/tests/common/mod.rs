//! Shared `dynamodb-local` setup for the DynamoDB conformance suites.
//!
//! Lives here rather than being copy-pasted into each test binary because the
//! skip policy below is the whole point, and a policy duplicated in two files is
//! a policy that gets fixed in one of them.
//!
//! **Why this exists.** The suites flaked with `create_table: dispatch failure`
//! — locally about 1 run in 3, and in CI on #328, #359 and #364. The old helper
//! only skipped when the container failed to *start* (Docker absent); once it
//! started, the first SDK call propagated its error and failed the test.
//!
//! The failure is a startup race, and not the one it looks like:
//! `WaitFor::message_on_stdout("Initializing DynamoDB Local")` fires when that
//! line is logged, but dynamodb-local logs it *before* it binds its port.
//! Meanwhile Docker's port proxy accepts TCP connections on the mapped port
//! immediately, whether or not anything is listening inside the container — so a
//! plain TCP probe passes while the SDK's first real request is still reset on
//! the spot. That instant reset is what the SDK reports as a dispatch failure
//! (which is why it fails in ~1.7s rather than after a timeout).
//!
//! So the readiness probe has to be a real request. `create_table` is that
//! probe: it is retried while — and only while — it fails to *reach* the
//! endpoint. Any other error, including a genuine schema or validation failure,
//! is returned immediately and fails the test. Connection establishment is the
//! only thing that earns a skip.

use std::time::{Duration, Instant};

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage};

use smooth_operator_adapter_dynamodb::DynamoDbAdapter;

/// How long the container gets to start answering requests before we treat it as
/// unusable and skip. Generous enough for a cold OrbStack/CI daemon, short enough
/// that a genuinely dead container doesn't stall the suite.
const READY_TIMEOUT: Duration = Duration::from_secs(30);

/// Delay between readiness attempts.
const RETRY_INTERVAL: Duration = Duration::from_millis(250);

/// Start `dynamodb-local` and return an adapter pointed at it.
///
/// `Ok(None)` means **skip**: either Docker is unavailable, or the container
/// never started answering requests. `Err` means a real failure worth failing
/// the test over.
pub async fn start() -> anyhow::Result<Option<(ContainerAsync<GenericImage>, DynamoDbAdapter)>> {
    let image = GenericImage::new("amazon/dynamodb-local", "latest")
        .with_wait_for(WaitFor::message_on_stdout("Initializing DynamoDB Local"))
        .with_exposed_port(8000.tcp());

    let node = match image.start().await {
        Ok(node) => node,
        Err(e) => {
            eprintln!("SKIP: could not start dynamodb-local container (Docker unavailable?): {e}");
            return Ok(None);
        }
    };

    let host = node.get_host().await?;
    let port = node.get_host_port_ipv4(8000).await?;
    let endpoint = format!("http://{host}:{port}");

    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        match connect(&endpoint).await {
            Ok(adapter) => return Ok(Some((node, adapter))),
            // Couldn't reach the endpoint — the container is still coming up.
            Err(e) if is_unreachable(&e) => {
                if Instant::now() >= deadline {
                    eprintln!(
                        "SKIP: dynamodb-local at {endpoint} never answered within {}s \
                         (container/network startup, not a code failure): {e}",
                        READY_TIMEOUT.as_secs()
                    );
                    return Ok(None);
                }
                tokio::time::sleep(RETRY_INTERVAL).await;
            }
            // Reached it and it said no — a real failure. Fail the test.
            Err(e) => return Err(e),
        }
    }
}

/// Is this error "couldn't get bytes to the endpoint" rather than "the service
/// rejected the request"? `dispatch failure` is the AWS SDK's class for the
/// former (connect refused/reset, DNS, TLS); a service error carries the API's
/// own message instead and must never be retried away into a skip.
fn is_unreachable(e: &anyhow::Error) -> bool {
    let msg = format!("{e:#}").to_ascii_lowercase();
    msg.contains("dispatch failure")
        || msg.contains("connection refused")
        || msg.contains("connection reset")
}

/// Build an adapter pointed at DynamoDB-Local with dummy static credentials
/// (DynamoDB-Local ignores them but the SDK requires *some* credentials).
/// `create_table` is idempotent (ResourceInUseException → Ok), so this is safe
/// to call repeatedly while waiting for readiness.
async fn connect(endpoint: &str) -> anyhow::Result<DynamoDbAdapter> {
    // SAFETY: these are dummy creds for a throwaway local container; the SDK only
    // needs them to be present. Set before building the adapter's AWS config.
    std::env::set_var("AWS_ACCESS_KEY_ID", "test");
    std::env::set_var("AWS_SECRET_ACCESS_KEY", "test");
    std::env::set_var("AWS_REGION", "us-east-1");
    std::env::set_var("SMOOTH_AGENT_DDB_TABLE", "smooth-operator-test");

    let adapter = DynamoDbAdapter::from_env(Some(endpoint)).await?;
    adapter.create_table().await?;
    Ok(adapter)
}

#[cfg(test)]
mod tests {
    use super::is_unreachable;

    /// The classifier is the whole safety boundary: misclassify a service error
    /// as "unreachable" and a real failure gets retried into a silent skip.
    #[test]
    fn classifies_only_connection_failures_as_unreachable() {
        // Reachability failures — retry, then skip.
        assert!(is_unreachable(&anyhow::anyhow!(
            "create_table: dispatch failure"
        )));
        assert!(is_unreachable(&anyhow::anyhow!(
            "tcp connect error: Connection refused (os error 61)"
        )));
        assert!(is_unreachable(&anyhow::anyhow!("Connection reset by peer")));

        // Service errors — the endpoint answered. These must stay FATAL.
        assert!(!is_unreachable(&anyhow::anyhow!(
            "create_table: ValidationException: Invalid KeySchema"
        )));
        assert!(!is_unreachable(&anyhow::anyhow!(
            "create_table: ResourceNotFoundException: table missing"
        )));
        assert!(!is_unreachable(&anyhow::anyhow!("AccessDeniedException")));
    }
}
