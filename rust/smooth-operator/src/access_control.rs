//! Document-level access control (feature gap G3).
//!
//! Org isolation already exists (every conversation / knowledge row carries an
//! `organizationId`, and the Postgres knowledge base filters on it). This module
//! adds the **within-org user/group entitlement** layer the industry calls
//! document-level permissions: even inside one organization, a document may be
//! restricted to specific users or groups, and a retrieval must only ever return
//! documents the requester is entitled to read.
//!
//! ## Why enforcement lives in our layer
//!
//! smooth-operator's [`KnowledgeBase`](smooth_operator_core::KnowledgeBase) trait is
//! upstream and **read-only to us**: its `query` returns a
//! [`KnowledgeResult`](smooth_operator_core::KnowledgeResult) that carries only
//! `document_id` / `chunk` / `score` / `source` — *not* the stored metadata —
//! and the in-memory backend drops document metadata on ingest entirely. So we
//! cannot read an ACL back out of a query result. Instead this module:
//!
//! 1. Records the document → [`DocAcl`] mapping **at ingest** (parsed from the
//!    [`DocAcl::ACL_METADATA_KEY`] metadata the document carries) into a side
//!    table the [`AclKnowledgeStore`] owns, while forwarding the document
//!    unchanged to the inner backend.
//! 2. **Filters at read**: an [`AclReader`] bound to the requester's
//!    [`AccessContext`] over-fetches from the inner backend, looks each result's
//!    ACL up in the side table, and drops any the requester cannot access before
//!    truncating to the requested `K`.
//!
//! This is backend-agnostic: the same [`AclKnowledgeStore`] wraps the in-memory,
//! Postgres, or DynamoDB knowledge base identically (the post-filter happens in
//! our layer, after the backend's own org-scoped query).
//!
//! ## No-ACL default semantics — **no-acl ⇒ org-public**
//!
//! A document ingested **without** an ACL (the legacy / existing-seed path) has
//! no entry in the side table and is treated as **org-public**: visible to
//! anyone whose query reaches it (org isolation already happened upstream). This
//! keeps existing seeded knowledge retrievable and makes ACLs strictly additive
//! — you opt a document *into* restriction by attaching a [`DocAcl`]. An
//! explicit `DocAcl::default()` (all fields empty, `public: false`) is the
//! opposite: a fully-locked document only its listed users/groups can read.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use smooth_operator_core::{Document, KnowledgeBase, KnowledgeResult};

/// Over-fetch multiplier: the inner backend is queried for `limit * this` (with
/// a floor) candidates so that, after dropping results the requester can't
/// access, the post-filter top-K is still full whenever enough accessible
/// documents exist. Mirrors the over-fetch the Postgres backend already does for
/// RRF fusion.
const OVERFETCH_FACTOR: usize = 5;

/// Lower bound on the candidate pool, so a small `limit` still over-fetches
/// enough to survive filtering.
const OVERFETCH_FLOOR: usize = 20;

/// A document's allow-list — who may read it within the organization.
///
/// A requester may read the document when **any** of these hold:
/// - the document is [`public`](DocAcl::public),
/// - the requester's `user_id` is in [`users`](DocAcl::users),
/// - any of the requester's groups is in [`groups`](DocAcl::groups).
///
/// The default (`public: false`, empty `users`/`groups`) is a fully-locked
/// document. Note that "no `DocAcl` recorded at all" is *different* — that is
/// org-public (see the module-level no-ACL default semantics).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocAcl {
    /// When true, any requester reaching this document may read it.
    #[serde(default)]
    pub public: bool,
    /// User ids explicitly allowed to read this document.
    #[serde(default)]
    pub users: Vec<String>,
    /// Group ids explicitly allowed to read this document.
    #[serde(default)]
    pub groups: Vec<String>,
}

impl DocAcl {
    /// The document-metadata key under which a [`DocAcl`] is serialized (as
    /// JSON) so it survives the trip through the ingestion pipeline and into the
    /// [`AclKnowledgeStore`]'s side table.
    pub const ACL_METADATA_KEY: &'static str = "acl_v2";

    /// A document readable by anyone reaching it.
    #[must_use]
    pub fn public() -> Self {
        Self {
            public: true,
            ..Self::default()
        }
    }

    /// A document readable only by the listed users.
    #[must_use]
    pub fn for_users<I, S>(users: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            public: false,
            users: users.into_iter().map(Into::into).collect(),
            groups: Vec::new(),
        }
    }

    /// A document readable only by members of the listed groups.
    #[must_use]
    pub fn for_groups<I, S>(groups: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            public: false,
            users: Vec::new(),
            groups: groups.into_iter().map(Into::into).collect(),
        }
    }

    /// Allow these additional users (builder).
    #[must_use]
    pub fn with_users<I, S>(mut self, users: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.users.extend(users.into_iter().map(Into::into));
        self
    }

    /// Allow these additional groups (builder).
    #[must_use]
    pub fn with_groups<I, S>(mut self, groups: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.groups.extend(groups.into_iter().map(Into::into));
        self
    }

    /// Serialize this ACL into a document's metadata under
    /// [`ACL_METADATA_KEY`](DocAcl::ACL_METADATA_KEY) (builder over a
    /// [`Document`]). This is how a connector / ingest path stamps an ACL onto a
    /// document so the [`AclKnowledgeStore`] records it.
    ///
    /// # Panics
    /// Never — [`DocAcl`] always serializes to JSON.
    #[must_use]
    pub fn attach_to(&self, doc: Document) -> Document {
        let json = serde_json::to_string(self).expect("DocAcl always serializes");
        doc.with_metadata(Self::ACL_METADATA_KEY, json)
    }

    /// Parse a [`DocAcl`] out of a document's metadata, if one is present and
    /// well-formed. Returns `None` when the key is absent (no-ACL ⇒ org-public)
    /// or the value fails to parse (treated as absent so a malformed stamp can't
    /// silently lock or unlock a document — it falls back to the default).
    #[must_use]
    pub fn from_metadata(metadata: &HashMap<String, String>) -> Option<Self> {
        let raw = metadata.get(Self::ACL_METADATA_KEY)?;
        serde_json::from_str(raw).ok()
    }
}

/// The identity a retrieval is performed *as* — the requester's entitlements.
///
/// Built from the authenticated user and the groups they belong to (resolved
/// upstream from the auth context). Passed into the knowledge-retrieval path so
/// results can be filtered by [`AccessContext::can_access`].
///
/// ## Org scoping ([`organization_id`](Self::organization_id))
///
/// The within-org user/group ACL ([`can_access`](Self::can_access)) is the
/// operator's single-tenant default and does **not** consult the org. A
/// **multi-tenant relational host** (e.g. SmooAI), however, needs the turn's org
/// to scope RAG to that tenant's documents — its
/// [`StorageAdapter::knowledge_for_access`](crate::adapter::StorageAdapter::knowledge_for_access)
/// reads `access.organization_id` to pick the right tenant before any
/// user/group filtering. So the org rides on the `AccessContext` purely to be
/// **available** to a host adapter; the built-in ACL path ignores it (org
/// isolation already happened upstream — every knowledge row carries an
/// `organizationId` the backend filters on). `None` ⇒ "no org resolved", which a
/// single-tenant adapter treats exactly as today.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccessContext {
    /// The requester's user id, if authenticated as a user. `None` for an
    /// anonymous / system requester (which then only sees public + no-ACL docs).
    pub user_id: Option<String>,
    /// The groups the requester belongs to.
    pub groups: Vec<String>,
    /// The organization this turn is scoped to, when resolved. Carried so a
    /// multi-tenant host adapter's `knowledge_for_access` can scope retrieval to
    /// the right tenant; the operator's built-in ACL ignores it (see the
    /// type-level "Org scoping" note). `None` when no org is resolved (the
    /// single-tenant / anonymous default).
    pub organization_id: Option<String>,
}

impl AccessContext {
    /// Build a context from an optional user id and a set of groups. The org is
    /// left unset (`None`); use [`with_organization_id`](Self::with_organization_id)
    /// to attach the turn's org for a multi-tenant host.
    #[must_use]
    pub fn new(user_id: Option<String>, groups: Vec<String>) -> Self {
        Self {
            user_id,
            groups,
            organization_id: None,
        }
    }

    /// A context for a specific user with no group memberships.
    #[must_use]
    pub fn for_user(user_id: impl Into<String>) -> Self {
        Self {
            user_id: Some(user_id.into()),
            groups: Vec::new(),
            organization_id: None,
        }
    }

    /// An anonymous requester: no user id, no groups. Sees only public and
    /// no-ACL (org-public) documents.
    #[must_use]
    pub fn anonymous() -> Self {
        Self::default()
    }

    /// Add a group membership (builder).
    #[must_use]
    pub fn with_group(mut self, group: impl Into<String>) -> Self {
        self.groups.push(group.into());
        self
    }

    /// Attach the turn's organization (builder). Carried so a multi-tenant host
    /// adapter's
    /// [`knowledge_for_access`](crate::adapter::StorageAdapter::knowledge_for_access)
    /// can scope retrieval to that tenant. The operator's built-in ACL ignores
    /// it (see the type-level "Org scoping" note), so this is behavior-preserving
    /// for the single-tenant default.
    #[must_use]
    pub fn with_organization_id(mut self, organization_id: impl Into<String>) -> Self {
        self.organization_id = Some(organization_id.into());
        self
    }

    /// Whether this requester may read a document with the given [`DocAcl`].
    ///
    /// `true` when the doc is public, or the requester's user id is in the
    /// allow-list, or any of the requester's groups is in the allow-list.
    #[must_use]
    pub fn can_access(&self, acl: &DocAcl) -> bool {
        if acl.public {
            return true;
        }
        if let Some(uid) = &self.user_id {
            if acl.users.iter().any(|u| u == uid) {
                return true;
            }
        }
        self.groups.iter().any(|g| acl.groups.contains(g))
    }
}

/// Side table mapping a stored `document_id` to its [`DocAcl`]. Shared (`Arc`)
/// between the ingest handle that populates it and every per-request reader that
/// consults it. Documents absent from the table are org-public (no-ACL default).
type AclTable = Arc<RwLock<HashMap<String, DocAcl>>>;

/// An ACL-aware knowledge store: wraps any inner
/// [`KnowledgeBase`](smooth_operator_core::KnowledgeBase) and records document ACLs
/// at ingest so retrieval can be filtered per requester.
///
/// Construction does **not** itself implement `KnowledgeBase` for reading,
/// because reads must be bound to a requester. Instead:
/// - [`ingest_handle`](AclKnowledgeStore::ingest_handle) returns an
///   `Arc<dyn KnowledgeBase>` that records ACLs as it ingests (used by the
///   ingestion pipeline);
/// - [`reader`](AclKnowledgeStore::reader) mints an ACL-filtering
///   `Arc<dyn KnowledgeBase>` bound to a specific [`AccessContext`] (used by the
///   runtime + `knowledge_search` tool for a turn).
#[derive(Clone)]
pub struct AclKnowledgeStore {
    inner: Arc<dyn KnowledgeBase>,
    acls: AclTable,
}

impl AclKnowledgeStore {
    /// Wrap an inner knowledge base. The store starts with an empty ACL table;
    /// every document ingested through [`ingest_handle`](Self::ingest_handle)
    /// that carries a [`DocAcl`] in its metadata is recorded.
    #[must_use]
    pub fn new(inner: Arc<dyn KnowledgeBase>) -> Self {
        Self {
            inner,
            acls: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// An ingest-side handle: a [`KnowledgeBase`] whose `ingest` records the
    /// document's ACL (if any) in the shared side table, then forwards to the
    /// inner backend. Its `query` is the **unfiltered** inner query (callers
    /// that read for a specific requester use [`reader`](Self::reader) instead).
    #[must_use]
    pub fn ingest_handle(&self) -> Arc<dyn KnowledgeBase> {
        Arc::new(AclIngestHandle {
            inner: Arc::clone(&self.inner),
            acls: Arc::clone(&self.acls),
        })
    }

    /// A read-side handle bound to `ctx`: a [`KnowledgeBase`] whose `query`
    /// over-fetches from the inner backend and drops every result the requester
    /// is not entitled to before truncating to the requested limit.
    #[must_use]
    pub fn reader(&self, ctx: AccessContext) -> Arc<dyn KnowledgeBase> {
        Arc::new(AclReader {
            inner: Arc::clone(&self.inner),
            acls: Arc::clone(&self.acls),
            ctx,
        })
    }

    /// Record `document_id → acl` directly (without ingesting a document) — for
    /// callers that store documents through some other path but still want the
    /// ACL enforced at read.
    ///
    /// # Errors
    /// Returns an error if the ACL table lock is poisoned.
    pub fn record_acl(&self, document_id: impl Into<String>, acl: DocAcl) -> anyhow::Result<()> {
        let mut table = self
            .acls
            .write()
            .map_err(|e| anyhow::anyhow!("acl table lock poisoned: {e}"))?;
        table.insert(document_id.into(), acl);
        Ok(())
    }
}

/// Records ACLs at ingest, forwarding documents to the inner backend.
struct AclIngestHandle {
    inner: Arc<dyn KnowledgeBase>,
    acls: AclTable,
}

impl KnowledgeBase for AclIngestHandle {
    fn ingest(&self, doc: Document) -> anyhow::Result<()> {
        // Record the ACL (if the document carries one) keyed by document id, so
        // a later query result with that document_id can be access-checked.
        if let Some(acl) = DocAcl::from_metadata(&doc.metadata) {
            let mut table = self
                .acls
                .write()
                .map_err(|e| anyhow::anyhow!("acl table lock poisoned: {e}"))?;
            table.insert(doc.id.clone(), acl);
        }
        self.inner.ingest(doc)
    }

    fn query(&self, query: &str, limit: usize) -> anyhow::Result<Vec<KnowledgeResult>> {
        // Ingest handle reads are unfiltered (no requester bound). Production
        // reads go through `reader(ctx)`.
        self.inner.query(query, limit)
    }
}

/// Filters query results by a bound [`AccessContext`].
struct AclReader {
    inner: Arc<dyn KnowledgeBase>,
    acls: AclTable,
    ctx: AccessContext,
}

impl KnowledgeBase for AclReader {
    fn ingest(&self, doc: Document) -> anyhow::Result<()> {
        // A reader can still ingest (recording ACLs), so the same handle is
        // usable end to end in tests — but production ingest uses ingest_handle.
        if let Some(acl) = DocAcl::from_metadata(&doc.metadata) {
            let mut table = self
                .acls
                .write()
                .map_err(|e| anyhow::anyhow!("acl table lock poisoned: {e}"))?;
            table.insert(doc.id.clone(), acl);
        }
        self.inner.ingest(doc)
    }

    fn query(&self, query: &str, limit: usize) -> anyhow::Result<Vec<KnowledgeResult>> {
        // Over-fetch so the post-filter top-K is full whenever enough accessible
        // documents exist.
        let candidate_n = limit.saturating_mul(OVERFETCH_FACTOR).max(OVERFETCH_FLOOR);
        let candidates = self.inner.query(query, candidate_n)?;

        let table = self
            .acls
            .read()
            .map_err(|e| anyhow::anyhow!("acl table lock poisoned: {e}"))?;

        let mut out = Vec::with_capacity(limit.min(candidates.len()));
        for result in candidates {
            // No recorded ACL ⇒ org-public (backward-compatible default).
            let allowed = match table.get(&result.document_id) {
                Some(acl) => self.ctx.can_access(acl),
                None => true,
            };
            if allowed {
                out.push(result);
                if out.len() == limit {
                    break;
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- can_access matrix ----------------------------------------------

    #[test]
    fn can_access_public_allows_anyone() {
        let acl = DocAcl::public();
        assert!(AccessContext::anonymous().can_access(&acl));
        assert!(AccessContext::for_user("anyone").can_access(&acl));
    }

    #[test]
    fn can_access_user_match() {
        let acl = DocAcl::for_users(["alice"]);
        assert!(AccessContext::for_user("alice").can_access(&acl));
    }

    #[test]
    fn can_access_user_no_match_is_denied() {
        let acl = DocAcl::for_users(["alice"]);
        assert!(!AccessContext::for_user("bob").can_access(&acl));
        // Anonymous (no user id) is denied a user-restricted doc.
        assert!(!AccessContext::anonymous().can_access(&acl));
    }

    #[test]
    fn can_access_group_match() {
        let acl = DocAcl::for_groups(["support"]);
        let ctx = AccessContext::new(Some("carol".into()), vec!["support".into()]);
        assert!(ctx.can_access(&acl));
    }

    #[test]
    fn can_access_group_no_match_is_denied() {
        let acl = DocAcl::for_groups(["support"]);
        let ctx = AccessContext::new(Some("dave".into()), vec!["billing".into()]);
        assert!(!ctx.can_access(&acl));
    }

    #[test]
    fn can_access_empty_acl_is_fully_locked() {
        // An explicit, empty DocAcl (public:false, no users/groups) denies all —
        // this is distinct from "no DocAcl recorded" (which is org-public).
        let acl = DocAcl::default();
        assert!(!AccessContext::for_user("alice").can_access(&acl));
        assert!(!AccessContext::anonymous().can_access(&acl));
        let grouped = AccessContext::new(Some("x".into()), vec!["g".into()]);
        assert!(!grouped.can_access(&acl));
    }

    // ---- organization_id threading --------------------------------------

    #[test]
    fn organization_id_defaults_none_and_builder_sets_it() {
        // Existing constructors leave the org unset (behavior-preserving).
        assert_eq!(AccessContext::new(None, vec![]).organization_id, None);
        assert_eq!(AccessContext::for_user("u").organization_id, None);
        assert_eq!(AccessContext::anonymous().organization_id, None);
        // The builder attaches the turn's org.
        let ctx = AccessContext::for_user("u").with_organization_id("org-x");
        assert_eq!(ctx.organization_id, Some("org-x".to_string()));
    }

    #[test]
    fn organization_id_does_not_affect_can_access() {
        // Org is for the host adapter's scoping, not the within-org ACL: it must
        // not change the public/user/group decision.
        let acl = DocAcl::for_users(["alice"]);
        let with_org = AccessContext::for_user("alice").with_organization_id("org-x");
        assert!(with_org.can_access(&acl));
        let other_org = AccessContext::for_user("bob").with_organization_id("org-x");
        assert!(!other_org.can_access(&acl));
    }

    #[test]
    fn can_access_mixed_user_or_group() {
        // public:false, but allows user alice OR group support — either grants.
        let acl = DocAcl::for_users(["alice"]).with_groups(["support"]);
        assert!(AccessContext::for_user("alice").can_access(&acl));
        let grp = AccessContext::new(Some("zed".into()), vec!["support".into()]);
        assert!(grp.can_access(&acl));
        let neither = AccessContext::new(Some("zed".into()), vec!["billing".into()]);
        assert!(!neither.can_access(&acl));
    }

    // ---- DocAcl (de)serialization round-trip ----------------------------

    #[test]
    fn docacl_round_trips_through_metadata() {
        let acl = DocAcl::for_users(["alice", "bob"]).with_groups(["support"]);
        let doc = acl.attach_to(Document::new(
            "c",
            "s",
            smooth_operator_core::DocumentType::Documentation,
        ));
        let parsed = DocAcl::from_metadata(&doc.metadata).expect("acl present");
        assert_eq!(parsed, acl);
    }

    #[test]
    fn from_metadata_absent_is_none() {
        let doc = Document::new("c", "s", smooth_operator_core::DocumentType::Documentation);
        assert!(DocAcl::from_metadata(&doc.metadata).is_none());
    }

    #[test]
    fn from_metadata_malformed_is_none() {
        let mut metadata = HashMap::new();
        metadata.insert(
            DocAcl::ACL_METADATA_KEY.to_string(),
            "{not json".to_string(),
        );
        assert!(DocAcl::from_metadata(&metadata).is_none());
    }
}
