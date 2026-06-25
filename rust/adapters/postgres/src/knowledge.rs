//! pgvector-backed [`KnowledgeBase`] with hybrid dense + sparse retrieval.
//!
//! smooth-operator's [`KnowledgeBase`](smooth_operator_core::KnowledgeBase) trait is
//! **synchronous** (the engine calls `ingest`/`query` directly), but both
//! embedding and Postgres access are async here. We bridge the two by `spawn`ing
//! the async work onto the captured runtime [`Handle`] (so its I/O makes
//! progress on that runtime's reactor) and blocking the calling thread on the
//! task's `JoinHandle` from a throwaway OS thread — never calling
//! `Handle::block_on` on a runtime worker thread (which panics "Cannot start a
//! runtime from within a runtime"). See [`PgKnowledgeBase::run_blocking`].
//!
//! ## Retrieval
//!
//! 1. **Dense**: embed the query, rank rows by pgvector cosine distance
//!    (`embedding <=> $query`), take the top-K.
//! 2. **Sparse**: `content_tsv @@ plainto_tsquery('english', $query)`, ranked by
//!    `ts_rank`, top-K.
//! 3. **Fuse**: Reciprocal Rank Fusion (RRF) over the two ranked lists —
//!    `score = Σ 1/(k + rank)` (k=60) — then return the top-K fused chunks.
//!
//! This mirrors smooai's `knowledge_vectors` retrieval (dense HNSW ∪ sparse BM25
//! → RRF).

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use deadpool_postgres::Pool;
use tokio::runtime::Handle;

use smooth_operator_core::{Document, KnowledgeBase, KnowledgeResult};

use smooth_operator::access_control::{AccessContext, DocAcl};
use smooth_operator::embedding::{Embedder, InputType};

/// RRF constant. 60 is the canonical value from the original RRF paper; it
/// damps the contribution of low-ranked items without ignoring them.
const RRF_K: f32 = 60.0;

/// pgvector knowledge base. Cheap to clone (all fields are `Arc`/pool handles).
#[derive(Clone)]
pub struct PgKnowledgeBase {
    pool: Pool,
    embedder: Arc<dyn Embedder>,
    handle: Handle,
    /// Optional org scoping. When set, ingest stamps and query filters on it.
    organization_id: Option<String>,
    /// Optional document-level access control (feature gap G3). When set, every
    /// query filters rows by this requester's entitlements against the stored
    /// `acl` column **in SQL** (so a restricted document is never even fetched).
    /// `None` ⇒ no within-org ACL filtering (org isolation only) — the handle
    /// returned by [`StorageAdapter::knowledge`]. The chat path uses
    /// [`StorageAdapter::knowledge_for_access`], which sets this.
    access: Option<AccessContext>,
}

impl PgKnowledgeBase {
    pub(crate) fn new(
        pool: Pool,
        embedder: Arc<dyn Embedder>,
        handle: Handle,
        organization_id: Option<String>,
    ) -> Self {
        Self {
            pool,
            embedder,
            handle,
            organization_id,
            access: None,
        }
    }

    /// A clone of this knowledge base whose queries enforce the given
    /// [`AccessContext`]'s document-level entitlements (in SQL, against the
    /// stored `acl` column). Used by
    /// [`PostgresAdapter::knowledge_for_access`](crate::PostgresAdapter) on the
    /// chat retrieval path.
    ///
    /// When the context carries an [`organization_id`](AccessContext::organization_id)
    /// (a multi-tenant host threads the turn's org through), it **overrides** the
    /// adapter's construction-time org for this query — so one adapter instance can
    /// serve per-turn tenants instead of being pinned to a single static org. The
    /// org is still a cheap SQL pre-filter (`organization_id = $1`). A context with
    /// no org leaves the construction-time org unchanged, so the single-tenant
    /// default is behavior-preserving.
    #[must_use]
    pub fn with_access(&self, access: AccessContext) -> Self {
        let organization_id = access
            .organization_id
            .clone()
            .or_else(|| self.organization_id.clone());
        Self {
            organization_id,
            access: Some(access),
            ..self.clone()
        }
    }

    /// Format a vector as a pgvector literal: `[0.1,0.2,...]`.
    fn vector_literal(v: &[f32]) -> String {
        let mut s = String::with_capacity(v.len() * 8 + 2);
        s.push('[');
        for (i, x) in v.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&x.to_string());
        }
        s.push(']');
        s
    }

    async fn ingest_async(&self, doc: Document) -> Result<()> {
        let embeddings = self
            .embedder
            .embed(std::slice::from_ref(&doc.content), InputType::Document)
            .await?;
        let embedding = embeddings
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("embedder returned no vector"))?;
        let literal = Self::vector_literal(&embedding);
        let metadata = serde_json::to_value(&doc.metadata)?;
        // Persist the document's ACL (feature gap G3) as a discrete column so it
        // survives the ingest→serve process boundary and can be filtered in SQL
        // at read. Parsed from the same `acl_v2` metadata key the in-memory
        // store records. `None` ⇒ NULL ⇒ org-public (backward-compatible).
        let acl: Option<serde_json::Value> = DocAcl::from_metadata(&doc.metadata)
            .map(|a| serde_json::to_value(&a))
            .transpose()?;
        // Stable per-chunk id: the document is stored as a single chunk keyed by
        // its document id, so re-ingesting the same doc upserts in place.
        let row_id = doc.id.clone();

        let client = self.pool.get().await?;
        client
            .execute(
                "INSERT INTO knowledge_vectors
                    (id, document_id, organization_id, source, content, embedding, metadata, acl)
                 VALUES ($1, $2, $3, $4, $5, $6::text::vector, $7, $8)
                 ON CONFLICT (id) DO UPDATE SET
                    document_id     = EXCLUDED.document_id,
                    organization_id = EXCLUDED.organization_id,
                    source          = EXCLUDED.source,
                    content         = EXCLUDED.content,
                    embedding       = EXCLUDED.embedding,
                    metadata        = EXCLUDED.metadata,
                    acl             = EXCLUDED.acl",
                &[
                    &row_id,
                    &doc.id,
                    &self.organization_id,
                    &doc.source,
                    &doc.content,
                    &literal,
                    &metadata,
                    &acl,
                ],
            )
            .await?;
        Ok(())
    }

    async fn query_async(&self, query: &str, limit: usize) -> Result<Vec<KnowledgeResult>> {
        let embeddings = self
            .embedder
            .embed(&[query.to_string()], InputType::Query)
            .await?;
        let embedding = embeddings
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("embedder returned no query vector"))?;
        let literal = Self::vector_literal(&embedding);

        // Pull a generous candidate pool from each arm so RRF has something to
        // fuse, then truncate after fusion.
        let candidate_n: i64 = i64::try_from((limit * 4).max(20)).unwrap_or(20);
        let client = self.pool.get().await?;

        // --- ACL filter (feature gap G3) ---
        //
        // When this handle is access-bound, every row must pass the requester's
        // document-level entitlement **in SQL** — a restricted document is never
        // even fetched. A row is visible when ANY holds:
        //   - `acl IS NULL`              → no ACL recorded ⇒ org-public default
        //   - `acl->>'public'` is true   → explicitly public
        //   - requester user id ∈ acl.users   (jsonb `?` key-exists)
        //   - any requester group ∈ acl.groups (jsonb `?|` any-key-exists)
        // `$4` is the requester user id (text, NULL ⇒ anonymous), `$5` the
        // requester groups (text[]). Both are bound below. When the handle is
        // NOT access-bound (`access` is None) the predicate is `TRUE` — org
        // isolation only, no within-org filtering.
        // Build the ACL predicate + the extra bound params ONLY when this handle
        // is access-bound. Postgres rejects a prepared statement that binds a
        // parameter the SQL never references, so the raw (org-isolation-only)
        // path must not add `$4`/`$5`.
        let acl_user: Option<String> = self.access.as_ref().and_then(|c| c.user_id.clone());
        let acl_groups: Vec<String> = self
            .access
            .as_ref()
            .map(|c| c.groups.clone())
            .unwrap_or_default();
        let acl_predicate = if self.access.is_some() {
            // A row is visible when it has no recorded ACL (org-public), is
            // explicitly public, names the requester's user id, or names any of
            // the requester's groups. `?` / `?|` are jsonb key-exists operators.
            "(acl IS NULL \
              OR (acl->>'public')::boolean IS TRUE \
              OR ($4::text IS NOT NULL AND acl->'users' ? $4) \
              OR (acl->'groups' ?| $5::text[]))"
        } else {
            "TRUE"
        };
        // The query text as an owned param so the borrowed trait objects below
        // don't tie the param vec to the input `&str` lifetime.
        let query_owned = query.to_string();

        // --- dense arm: cosine distance via pgvector `<=>` ---
        let dense_sql = format!(
            "SELECT id, document_id, source, content
             FROM knowledge_vectors
             WHERE ($1::text IS NULL OR organization_id = $1)
               AND {acl_predicate}
             ORDER BY embedding <=> $2::text::vector
             LIMIT $3"
        );
        let mut dense_params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
            vec![&self.organization_id, &literal, &candidate_n];
        if self.access.is_some() {
            dense_params.push(&acl_user);
            dense_params.push(&acl_groups);
        }
        let dense_rows = client.query(&dense_sql, &dense_params).await?;

        // --- sparse arm: tsvector BM25-style match, ranked by ts_rank ---
        let sparse_sql = format!(
            "SELECT id, document_id, source, content
             FROM knowledge_vectors
             WHERE ($1::text IS NULL OR organization_id = $1)
               AND content_tsv @@ plainto_tsquery('english', $2)
               AND {acl_predicate}
             ORDER BY ts_rank(content_tsv, plainto_tsquery('english', $2)) DESC
             LIMIT $3"
        );
        let mut sparse_params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
            vec![&self.organization_id, &query_owned, &candidate_n];
        if self.access.is_some() {
            sparse_params.push(&acl_user);
            sparse_params.push(&acl_groups);
        }
        let sparse_rows = client.query(&sparse_sql, &sparse_params).await?;

        // --- Reciprocal Rank Fusion ---
        struct Hit {
            document_id: String,
            source: String,
            content: String,
            score: f32,
        }
        let mut fused: HashMap<String, Hit> = HashMap::new();

        let mut fuse = |rows: &[tokio_postgres::Row]| {
            for (rank, row) in rows.iter().enumerate() {
                let id: String = row.get(0);
                let document_id: String = row.get(1);
                let source: String = row.get(2);
                let content: String = row.get(3);
                #[allow(clippy::cast_precision_loss)]
                let contribution = 1.0 / (RRF_K + (rank as f32) + 1.0);
                fused
                    .entry(id)
                    .and_modify(|h| h.score += contribution)
                    .or_insert(Hit {
                        document_id,
                        source,
                        content,
                        score: contribution,
                    });
            }
        };
        fuse(&dense_rows);
        fuse(&sparse_rows);

        let mut hits: Vec<Hit> = fused.into_values().collect();
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(limit);

        Ok(hits
            .into_iter()
            .map(|h| KnowledgeResult {
                document_id: h.document_id,
                chunk: h.content,
                score: h.score,
                source: h.source,
            })
            .collect())
    }
}

impl PgKnowledgeBase {
    /// Drive an async future to completion from a *synchronous* trait method.
    ///
    /// `KnowledgeBase` is sync, but our work (embedding + deadpool) is async.
    /// `Handle::block_on` can't be called from a runtime worker thread (it panics
    /// "Cannot start a runtime from within a runtime"), and `block_in_place` only
    /// relieves the *blocking-budget* concern, not that one. So we `spawn` the
    /// future onto the runtime (where it can make progress) and block the calling
    /// thread on a oneshot channel — wrapped in `block_in_place` when we happen to
    /// be on a multi-thread worker so we don't starve the scheduler.
    fn run_blocking<F, T>(&self, fut: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        // Spawn the real work onto the captured runtime so its async I/O
        // (deadpool, embedding HTTP) makes progress on that runtime's reactor.
        let join = self.handle.spawn(fut);

        // Block on the JoinHandle from a throwaway OS thread that owns a tiny
        // current-thread runtime. This never calls `Handle::block_on` on a worker
        // thread (which panics "Cannot start a runtime from within a runtime"),
        // so it's safe whether the caller is on a runtime worker or not.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = (|| -> Result<T> {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                let joined = rt.block_on(join);
                joined.map_err(|e| anyhow!("knowledge task panicked or was cancelled: {e}"))?
            })();
            let _ = tx.send(result);
        });
        rx.recv()
            .map_err(|e| anyhow!("knowledge task channel closed: {e}"))?
    }
}

impl KnowledgeBase for PgKnowledgeBase {
    fn ingest(&self, doc: Document) -> Result<()> {
        let this = self.clone();
        self.run_blocking(async move { this.ingest_async(doc).await })
    }

    fn query(&self, query: &str, limit: usize) -> Result<Vec<KnowledgeResult>> {
        let this = self.clone();
        let query = query.to_string();
        self.run_blocking(async move { this.query_async(&query, limit).await })
    }
}
