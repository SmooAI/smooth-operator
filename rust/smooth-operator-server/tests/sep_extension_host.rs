//! SEP extension hosting on the operator server — the live-wire integration test.
//!
//! Spawns a real extension subprocess (the dependency-free conformance echo peer,
//! `node spec/extension/conformance/echo.mjs`) through the engine
//! [`ExtensionHost`], and asserts the server's composition claim: an extension's
//! tools reach the turn's [`ToolRegistry`] and flow through the SAME per-agent
//! `enabled_tools` retain the runner applies (SMOODEV-590), so an allow-list
//! drops an extension tool exactly like it drops a built-in.
//!
//! The `ui/confirm` → confirmation-frame bridge and the trust allow-list parse
//! are covered by the crate's unit tests (`src/extensions.rs`) without a
//! subprocess. This test is **skipped, not failed,** when `node` is not on PATH
//! (the Rust CI lane may not install it).

use std::path::PathBuf;
use std::sync::Arc;

use smooth_operator_core::extension::protocol::{HostInfo, WorkspaceInfo};
use smooth_operator_core::extension::{discover, DefaultHostDelegate, ExtensionHost, HostDelegate};
use smooth_operator_core::ToolRegistry;

/// Absolute path to the dependency-free conformance echo peer shipped in `spec/`.
fn echo_peer_mjs() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../spec/extension/conformance/echo.mjs")
        .canonicalize()
        .expect("echo.mjs exists")
}

fn node_available() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Write a `<dir>/echo/extension.toml` that runs `node <echo.mjs>`.
fn write_echo_manifest(root: &std::path::Path) {
    let ext_dir = root.join("echo");
    std::fs::create_dir_all(&ext_dir).unwrap();
    let peer = echo_peer_mjs();
    let toml = format!(
        "name = \"echo\"\nversion = \"0.1.0\"\n[run]\ncommand = \"node\"\nargs = [\"{}\"]\n[capabilities]\ntools = true\n",
        peer.display()
    );
    std::fs::write(ext_dir.join("extension.toml"), toml).unwrap();
}

async fn load_echo_host(dir: &std::path::Path) -> ExtensionHost {
    let (discovered, failures) = discover(Some(dir), None);
    assert!(failures.is_empty(), "manifest parse failures: {failures:?}");
    assert_eq!(discovered.len(), 1, "expected exactly the echo extension");
    let delegate: Arc<dyn HostDelegate> = Arc::new(DefaultHostDelegate);
    let (host, load_failures) = ExtensionHost::load(
        discovered,
        HostInfo {
            name: "test".into(),
            version: "0".into(),
        },
        WorkspaceInfo {
            root: dir.to_string_lossy().into_owned(),
            trusted: true,
        },
        "widget",
        vec!["confirm".to_string()],
        delegate,
    )
    .await;
    assert!(
        load_failures.is_empty(),
        "extension load failures: {load_failures:?}"
    );
    host
}

/// Register `host.tools()` into a fresh registry, apply an `enabled_tools` retain
/// (as the runner does), and report whether the ext tool survived.
fn survives_enabled_tools(host: &ExtensionHost, enabled: &[&str]) -> bool {
    let mut registry = ToolRegistry::new();
    for t in host.tools() {
        registry.register_arc(t);
    }
    registry.retain(|name| enabled.contains(&name));
    registry.has_tool("echo.say")
}

#[tokio::test]
async fn extension_tool_reaches_registry_and_honors_enabled_tools() {
    if !node_available() {
        eprintln!("skipping sep_extension_host: `node` not on PATH");
        return;
    }
    let tmp = std::env::temp_dir().join(format!("sep-ext-host-{}", uuid::Uuid::new_v4()));
    write_echo_manifest(&tmp);

    let host = load_echo_host(&tmp).await;
    assert_eq!(
        host.names(),
        vec!["echo"],
        "the echo extension should be loaded"
    );

    // The extension's `say` tool surfaces as a dotted `echo.say` proxy.
    let tools = host.tools();
    assert!(
        tools.iter().any(|t| t.schema().name == "echo.say"),
        "echo.say missing from host.tools(); got {:?}",
        tools.iter().map(|t| t.schema().name).collect::<Vec<_>>()
    );

    // Registered into a turn registry with NO allow-list restriction, it's present.
    let mut registry = ToolRegistry::new();
    for t in host.tools() {
        registry.register_arc(t);
    }
    assert!(
        registry.has_tool("echo.say"),
        "ext tool should register into the turn registry"
    );

    // enabled_tools that INCLUDES the ext tool keeps it; one that EXCLUDES it
    // drops it — exactly the SMOODEV-590 filtering built-ins get.
    assert!(
        survives_enabled_tools(&host, &["echo.say"]),
        "ext tool must survive an allow-list that names it"
    );
    assert!(
        !survives_enabled_tools(&host, &["some_builtin"]),
        "ext tool must be filtered out when enabled_tools excludes it"
    );

    std::fs::remove_dir_all(&tmp).ok();
}
