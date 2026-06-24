//! Host-contributed tool injection seam.
//!
//! The reference runner assembles a fixed [`ToolRegistry`] of built-in tools
//! (`knowledge_search`, …) for every turn. A *host* — a deployment flavor that
//! embeds this runner (e.g. SmooAI's k8s flavor) — often needs to contribute
//! its OWN tools to a turn: per-org integrations, a CRM lookup, a ticketing
//! action, etc. Those tools depend on host-specific state (DB handles, an org's
//! connector config) that has no place in this shared crate.
//!
//! [`ToolProvider`] is the mechanism: a host installs one provider, and the
//! runner asks it — per turn, with the turn's [`ToolProviderContext`] — for the
//! extra tools to MERGE with the built-ins. The shared crate stays free of any
//! host/DB specifics; it only knows "ask the provider, register what it
//! returns". When no provider is installed the registry is exactly the
//! built-ins, so default behavior is byte-for-byte unchanged.
//!
//! ## Org-scoping
//!
//! [`ToolProviderContext`] carries the turn's [`AccessContext`] (the requester's
//! entitlements) and an optional `org_id`, so a provider can return per-org
//! tools and apply the requester's entitlements when wiring them. The shared
//! crate does not interpret `org_id` — it only carries it through.
//!
//! ## Per-turn handles
//!
//! Beyond org-scoping, a host's tools often need two more per-turn facts the
//! runner already has in hand: the turn's `conversation_id` (so a tool can
//! persist or correlate to the conversation it runs in) and the resolved
//! per-org `gateway_key` (so a retrieval-style host tool can call the same LLM
//! gateway this turn was billed/scoped to). Both are carried as `Option` and
//! the shared crate never interprets them — it only threads them through.

use std::sync::Arc;

use async_trait::async_trait;
use smooth_operator_core::Tool;

use crate::access_control::AccessContext;

/// The per-turn context a [`ToolProvider`] sees when asked for tools.
///
/// Carries everything a host needs to decide which tools a turn gets WITHOUT
/// leaking host/DB specifics into this crate: the requester's entitlements and
/// the (optional) owning org. A host keys its tool catalog off `org_id` and
/// scopes side-effectful tools to `access`.
#[derive(Debug, Clone, Default)]
pub struct ToolProviderContext {
    /// The owning organization for this turn, when known. `None` for a turn
    /// with no resolved org (e.g. an anonymous reference-server connection).
    pub org_id: Option<String>,
    /// The requester's document-level entitlements for this turn. A provider
    /// that returns retrieval-style tools should bind them to this context so a
    /// host tool never surfaces content the requester may not read.
    pub access: AccessContext,
    /// The conversation this turn belongs to, when known. A host tool that
    /// persists or correlates to the conversation it runs in reads this; `None`
    /// for a turn with no resolved conversation. The shared crate does not
    /// interpret it.
    pub conversation_id: Option<String>,
    /// The resolved per-org LLM-gateway key for this turn, when one was
    /// resolved. A retrieval-style host tool (e.g. agent-brain's
    /// `knowledge_search`) reads this to call the same gateway the turn was
    /// billed/scoped to. `None` when no key resolved. The shared crate does not
    /// interpret it.
    pub gateway_key: Option<String>,
}

impl ToolProviderContext {
    /// Build a context from an optional org id and the requester's access.
    ///
    /// The per-turn [`conversation_id`](Self::conversation_id) and
    /// [`gateway_key`](Self::gateway_key) default to `None`; set them with
    /// [`with_conversation_id`](Self::with_conversation_id) /
    /// [`with_gateway_key`](Self::with_gateway_key).
    #[must_use]
    pub fn new(org_id: Option<String>, access: AccessContext) -> Self {
        Self {
            org_id,
            access,
            conversation_id: None,
            gateway_key: None,
        }
    }

    /// Set the turn's [`conversation_id`](Self::conversation_id).
    #[must_use]
    pub fn with_conversation_id(mut self, conversation_id: impl Into<String>) -> Self {
        self.conversation_id = Some(conversation_id.into());
        self
    }

    /// Set the turn's resolved [`gateway_key`](Self::gateway_key).
    #[must_use]
    pub fn with_gateway_key(mut self, gateway_key: impl Into<String>) -> Self {
        self.gateway_key = Some(gateway_key.into());
        self
    }
}

/// Host seam for contributing EXTRA tools to a turn's [`ToolRegistry`].
///
/// The runner calls [`tools_for`](ToolProvider::tools_for) once per turn and
/// merges the returned tools with the built-ins (built-ins registered first;
/// a returned tool whose name collides with a built-in replaces it — the host
/// opted into that by naming it the same). Returning an empty `Vec` (or not
/// installing a provider at all) leaves the registry as exactly the built-ins.
///
/// Async so a provider may consult host state (config store, DB) to resolve an
/// org's tool catalog.
#[async_trait]
pub trait ToolProvider: Send + Sync {
    /// The extra tools to merge into this turn's registry. May be empty.
    async fn tools_for(&self, ctx: &ToolProviderContext) -> Vec<Arc<dyn Tool>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use smooth_operator_core::{ToolRegistry, ToolSchema};

    /// A trivial tool used to prove injected tools land in the registry.
    struct StubTool {
        name: String,
    }

    #[async_trait]
    impl Tool for StubTool {
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: self.name.clone(),
                description: "stub".into(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }
        async fn execute(&self, _arguments: serde_json::Value) -> anyhow::Result<String> {
            Ok("ok".into())
        }
    }

    /// A provider that returns a fixed set of stub tools.
    struct StubProvider {
        names: Vec<String>,
    }

    #[async_trait]
    impl ToolProvider for StubProvider {
        async fn tools_for(&self, _ctx: &ToolProviderContext) -> Vec<Arc<dyn Tool>> {
            self.names
                .iter()
                .map(|n| Arc::new(StubTool { name: n.clone() }) as Arc<dyn Tool>)
                .collect()
        }
    }

    #[tokio::test]
    async fn provider_tools_register_into_registry() {
        let provider = StubProvider {
            names: vec!["crm_lookup".into(), "open_ticket".into()],
        };
        let ctx = ToolProviderContext::new(Some("org-a".into()), AccessContext::anonymous());

        let mut registry = ToolRegistry::new();
        for tool in provider.tools_for(&ctx).await {
            registry.register_arc(tool);
        }

        assert!(registry.has_tool("crm_lookup"));
        assert!(registry.has_tool("open_ticket"));
    }

    #[test]
    fn new_defaults_per_turn_handles_to_none() {
        let ctx = ToolProviderContext::new(Some("org-a".into()), AccessContext::anonymous());
        assert_eq!(ctx.conversation_id, None);
        assert_eq!(ctx.gateway_key, None);
    }

    #[test]
    fn builder_sets_conversation_id_and_gateway_key() {
        let ctx = ToolProviderContext::new(Some("org-a".into()), AccessContext::anonymous())
            .with_conversation_id("conv-123")
            .with_gateway_key("sk-org-a");
        assert_eq!(ctx.conversation_id.as_deref(), Some("conv-123"));
        assert_eq!(ctx.gateway_key.as_deref(), Some("sk-org-a"));
    }

    #[tokio::test]
    async fn empty_provider_leaves_registry_unchanged() {
        let provider = StubProvider { names: vec![] };
        let ctx = ToolProviderContext::default();

        let mut registry = ToolRegistry::new();
        let before = registry.schemas().len();
        for tool in provider.tools_for(&ctx).await {
            registry.register_arc(tool);
        }
        assert_eq!(registry.schemas().len(), before);
    }
}
