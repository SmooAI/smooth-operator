//! Per-org LLM gateway-key resolution: the seam that lets a multi-tenant
//! deployment bill/scope each org's turns to its **own** gateway key while a
//! standalone/local server keeps using the single environment key.
//!
//! A turn runs against an OpenAI-compatible LLM gateway authenticated with a
//! gateway key (e.g. a per-org LiteLLM virtual key). The reference server reads
//! one key from `SMOOAI_GATEWAY_KEY` and uses it for every turn. A hosted,
//! multi-tenant flavor instead wants to resolve a **different** key per org so
//! usage is attributed and budgeted per tenant.
//!
//! This module is the **hook**: the public server defines the
//! [`GatewayKeyResolver`] trait + the default [`EnvGatewayKeyResolver`] (which
//! returns the single env key — the unchanged local/default behavior); the host
//! application plugs in a concrete resolver (e.g. backed by its per-org key
//! store) via `AppState::with_gateway_key_resolver`. No SmooAI/DB specifics live
//! here — only the trait and the env default.
//!
//! ## Resolution contract
//!
//! [`GatewayKeyResolver::resolve`] returns `Some(key)` to **override** the key
//! for that org, or `None` to **fall back** to the server's configured env key.
//! The per-turn LLM-config build always falls back to the env key on `None`, so
//! a resolver that only knows about a subset of orgs is safe — unknown orgs use
//! the env key exactly as today.

use std::sync::Arc;

use async_trait::async_trait;

/// Hook for resolving the LLM gateway key to use for a given org's turn.
///
/// Implemented by the host application (commonly backed by a per-org key store
/// — e.g. a LiteLLM virtual key per tenant). Returning `None` means "no
/// org-specific key" and the server falls back to its configured env key, so a
/// resolver that covers only some orgs is safe.
#[async_trait]
pub trait GatewayKeyResolver: Send + Sync {
    /// The gateway key to bill/scope this `org_id`'s turn to, or `None` to use
    /// the server's default (env) key.
    async fn resolve(&self, org_id: &str) -> Option<String>;
}

/// Default resolver: returns the single configured environment gateway key for
/// every org (the unchanged local/default behavior — no per-org scoping).
///
/// Constructed from the server's resolved gateway key. When the env key is
/// absent (`None`), this resolver returns `None` for every org, so the server
/// behaves exactly as it does today (a clean `LLM_UNAVAILABLE` error on a turn).
#[derive(Debug, Clone, Default)]
pub struct EnvGatewayKeyResolver {
    env_key: Option<String>,
}

impl EnvGatewayKeyResolver {
    /// Build the env resolver over the server's configured gateway key (the
    /// value of `SMOOAI_GATEWAY_KEY`, or `None` when unset).
    #[must_use]
    pub fn new(env_key: Option<String>) -> Self {
        Self { env_key }
    }
}

#[async_trait]
impl GatewayKeyResolver for EnvGatewayKeyResolver {
    async fn resolve(&self, _org_id: &str) -> Option<String> {
        self.env_key.clone()
    }
}

/// Resolve the gateway key for `org_id`, falling back to `env_key` when the
/// resolver returns `None`.
///
/// This is the single place the per-turn LLM-config build calls: inject any
/// [`GatewayKeyResolver`] and the env key, and get back the key the turn should
/// use, or `None` when neither the resolver nor the env supplies one (turn is
/// then unavailable). Keeping the fallback here means every flavor — and every
/// polyglot port — resolves identically.
pub async fn resolve_gateway_key(
    resolver: &Arc<dyn GatewayKeyResolver>,
    org_id: &str,
    env_key: Option<&str>,
) -> Option<String> {
    match resolver.resolve(org_id).await {
        Some(key) => Some(key),
        None => env_key.map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stub resolver that overrides a single org with a fixed key and returns
    /// `None` for any other org (so the env fallback is exercised).
    struct OneOrgResolver {
        org: String,
        key: String,
    }

    #[async_trait]
    impl GatewayKeyResolver for OneOrgResolver {
        async fn resolve(&self, org_id: &str) -> Option<String> {
            if org_id == self.org {
                Some(self.key.clone())
            } else {
                None
            }
        }
    }

    #[tokio::test]
    async fn env_resolver_returns_env_key_for_every_org() {
        let resolver = EnvGatewayKeyResolver::new(Some("env-key".to_string()));
        assert_eq!(resolver.resolve("org-a").await, Some("env-key".to_string()));
        assert_eq!(resolver.resolve("org-b").await, Some("env-key".to_string()));
    }

    #[tokio::test]
    async fn env_resolver_returns_none_when_env_absent() {
        let resolver = EnvGatewayKeyResolver::new(None);
        assert_eq!(resolver.resolve("org-a").await, None);
    }

    #[tokio::test]
    async fn injected_resolver_overrides_per_org() {
        let resolver: Arc<dyn GatewayKeyResolver> = Arc::new(OneOrgResolver {
            org: "org-a".to_string(),
            key: "org-a-key".to_string(),
        });
        // The covered org gets its own key (the env fallback is ignored).
        assert_eq!(
            resolve_gateway_key(&resolver, "org-a", Some("env-key")).await,
            Some("org-a-key".to_string())
        );
    }

    #[tokio::test]
    async fn falls_back_to_env_when_resolver_returns_none() {
        let resolver: Arc<dyn GatewayKeyResolver> = Arc::new(OneOrgResolver {
            org: "org-a".to_string(),
            key: "org-a-key".to_string(),
        });
        // An org the resolver doesn't cover falls back to the env key.
        assert_eq!(
            resolve_gateway_key(&resolver, "org-b", Some("env-key")).await,
            Some("env-key".to_string())
        );
    }

    #[tokio::test]
    async fn resolves_to_none_when_neither_resolver_nor_env_supply_a_key() {
        let resolver: Arc<dyn GatewayKeyResolver> = Arc::new(EnvGatewayKeyResolver::new(None));
        assert_eq!(resolve_gateway_key(&resolver, "org-a", None).await, None);
    }
}
