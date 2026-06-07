//! Binary entry point for the smooth-operator-agent WebSocket service.
//!
//! Reads configuration from the environment (see
//! [`smooth_operator_agent_server::config`]) and serves the `/ws` endpoint until killed.

use anyhow::Result;
use smooth_operator_agent_core::init_telemetry;
use smooth_operator_agent_server::config::ServerConfig;

#[tokio::main]
async fn main() -> Result<()> {
    // Tracing + OpenTelemetry GenAI export. Honors RUST_LOG; installs an OTLP
    // exporter only when OTEL_EXPORTER_OTLP_ENDPOINT is set, otherwise
    // local-only logging (no collector needed). Idempotent.
    init_telemetry();

    let config = ServerConfig::from_env();
    smooth_operator_agent_server::server::run(config).await
}
