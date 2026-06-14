//! pgvector-backed [`Memory`] — persistent + semantic cross-thread agent memory.
//!
//! Parity gap Phase 3 / SMOODEV-1470. The core only shipped
//! [`InMemoryMemory`](smooth_operator_core::InMemoryMemory) (a `Vec` behind a
//! `Mutex`, keyword recall, gone on restart). The general agent needs
//! cross-thread *user* memory that survives restarts and recalls by **semantic
//! similarity** — the TS side does this with a Postgres `store`/`store_vectors`
//! namespaced by `['memories', orgId, userId]`. [`PgMemory`] is the Rust
//! equivalent.
//!
//! ## Namespace scoping
//!
//! The core [`Memory`] trait's `recall(query, limit)` carries **no** org/user
//! scoping in its signature (same shape as [`KnowledgeBase`]'s `query`). So,
//! exactly like [`PgKnowledgeBase`](crate::PgKnowledgeBase) binds an
//! `organization_id`, a [`PgMemory`] instance is **bound to one
//! `(organization_id, user_id)` namespace at construction**. Every `store`
//! stamps the namespace onto the row; every `recall` filters on it in SQL
//! *before* ANN ranking. A `PgMemory` for org A can never recall org B's rows
//! (or another user's, when user-scoped). `user_id = None` ⇒ org-wide memory.
//!
//! ## Semantic recall
//!
//! `store` embeds the entry content (through the shared [`Embedder`] seam —
//! [`DeterministicEmbedder`] offline so tests need no network, [`GatewayEmbedder`]
//! live) and upserts the row. `recall` embeds the query and ranks the namespace's
//! rows by pgvector cosine distance (`embedding <=> $query`) under the HNSW index,
//! returning the top-K with `relevance` set to the cosine similarity (`1 -
//! distance`). Dimension handling matches `knowledge_vectors`: the `vector(N)`
//! column width comes from `embedder.dim()`.
//!
//! ## Sync trait over async work
//!
//! [`Memory`] is **synchronous** (the engine calls it directly) but embedding +
//! deadpool are async. We bridge identically to
//! [`PgKnowledgeBase`](crate::PgKnowledgeBase): spawn the async future onto the
//! captured runtime [`Handle`] and block on it from a throwaway OS thread — never
//! `Handle::block_on` on a runtime worker (which panics "Cannot start a runtime
//! from within a runtime"). See [`PgMemory::run_blocking`].

use std::sync::Arc;

use anyhow::{anyhow, Result};
use deadpool_postgres::Pool;
use tokio::runtime::Handle;

use smooth_operator_core::{Memory, MemoryEntry, MemoryType};

use smooth_operator::embedding::{Embedder, InputType};

/// pgvector-backed agent memory, bound to one `(organization_id, user_id)`
/// namespace. Cheap to clone (all fields are `Arc`/pool handles + small strings).
#[derive(Clone)]
pub struct PgMemory {
    pool: Pool,
    embedder: Arc<dyn Embedder>,
    handle: Handle,
    /// Org scope — required. Every `store` stamps it; every `recall` filters on it.
    organization_id: String,
    /// User scope — `None` ⇒ org-wide memory. When `Some`, recall is isolated to
    /// this user's rows (and org-wide rows are NOT mixed in — strict namespace
    /// match, mirroring the TS `['memories', orgId, userId]` key tuple).
    user_id: Option<String>,
}

impl PgMemory {
    /// Build a memory handle bound to `(organization_id, user_id)`. Pass
    /// `user_id = None` for org-wide memory.
    pub(crate) fn new(
        pool: Pool,
        embedder: Arc<dyn Embedder>,
        handle: Handle,
        organization_id: impl Into<String>,
        user_id: Option<String>,
    ) -> Self {
        Self {
            pool,
            embedder,
            handle,
            organization_id: organization_id.into(),
            user_id,
        }
    }

    /// Format a vector as a pgvector literal: `[0.1,0.2,...]`. Same wire shape as
    /// [`PgKnowledgeBase`](crate::PgKnowledgeBase).
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

    /// Serialize a [`MemoryType`] to its stored text form (the serde tag, e.g.
    /// `"ShortTerm"`). Round-trips through [`Self::memory_type_from_str`].
    fn memory_type_to_str(mt: MemoryType) -> Result<String> {
        // serde serializes a unit enum variant as a JSON string `"Variant"`;
        // strip the quotes for a clean column value.
        let json = serde_json::to_string(&mt)?;
        Ok(json.trim_matches('"').to_string())
    }

    /// Parse a stored `memory_type` text back into a [`MemoryType`].
    fn memory_type_from_str(s: &str) -> Result<MemoryType> {
        Ok(serde_json::from_str(&format!("\"{s}\""))?)
    }

    async fn store_async(&self, entry: MemoryEntry) -> Result<()> {
        let embeddings = self
            .embedder
            .embed(std::slice::from_ref(&entry.content), InputType::Document)
            .await?;
        let embedding = embeddings
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("embedder returned no vector"))?;
        let literal = Self::vector_literal(&embedding);
        let metadata = serde_json::to_value(&entry.metadata)?;
        let memory_type = Self::memory_type_to_str(entry.memory_type)?;

        let client = self.pool.get().await?;
        // Upsert by entry id so re-storing the same logical memory replaces it in
        // place (re-embedding content that may have changed).
        client
            .execute(
                "INSERT INTO memories
                    (id, organization_id, user_id, content, memory_type, relevance,
                     metadata, embedding, created_at, last_accessed)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8::text::vector, $9, $10)
                 ON CONFLICT (id) DO UPDATE SET
                    organization_id = EXCLUDED.organization_id,
                    user_id         = EXCLUDED.user_id,
                    content         = EXCLUDED.content,
                    memory_type     = EXCLUDED.memory_type,
                    relevance       = EXCLUDED.relevance,
                    metadata        = EXCLUDED.metadata,
                    embedding       = EXCLUDED.embedding,
                    last_accessed   = EXCLUDED.last_accessed",
                &[
                    &entry.id,
                    &self.organization_id,
                    &self.user_id,
                    &entry.content,
                    &memory_type,
                    &entry.relevance,
                    &metadata,
                    &literal,
                    &entry.created_at,
                    &entry.last_accessed,
                ],
            )
            .await?;
        Ok(())
    }

    async fn recall_async(&self, query: &str, limit: usize) -> Result<Vec<MemoryEntry>> {
        let embeddings = self
            .embedder
            .embed(&[query.to_string()], InputType::Query)
            .await?;
        let embedding = embeddings
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("embedder returned no query vector"))?;
        let literal = Self::vector_literal(&embedding);
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);

        let client = self.pool.get().await?;
        // Strict namespace match: org always; user_id matched with NULL-safe
        // equality (`IS NOT DISTINCT FROM`) so a `None`-scoped handle recalls
        // exactly the org-wide rows and a `Some`-scoped handle recalls exactly
        // that user's rows — neither leaks into the other. Ranking is pgvector
        // cosine distance under the HNSW index; `1 - distance` is the similarity
        // surfaced as `relevance`.
        let rows = client
            .query(
                "SELECT id, content, memory_type, metadata, created_at, last_accessed,
                        1 - (embedding <=> $3::text::vector) AS similarity
                 FROM memories
                 WHERE organization_id = $1
                   AND user_id IS NOT DISTINCT FROM $2
                 ORDER BY embedding <=> $3::text::vector
                 LIMIT $4",
                &[&self.organization_id, &self.user_id, &literal, &limit_i64],
            )
            .await?;

        rows.iter()
            .map(|row| {
                let memory_type =
                    Self::memory_type_from_str(row.get::<_, String>("memory_type").as_str())?;
                let metadata_json: serde_json::Value = row.get("metadata");
                let metadata = serde_json::from_value(metadata_json)?;
                #[allow(clippy::cast_possible_truncation)]
                let similarity = row.get::<_, f64>("similarity") as f32;
                Ok(MemoryEntry {
                    id: row.get("id"),
                    content: row.get("content"),
                    memory_type,
                    relevance: similarity,
                    metadata,
                    created_at: row.get("created_at"),
                    last_accessed: row.get("last_accessed"),
                })
            })
            .collect()
    }

    async fn forget_async(&self, id: &str) -> Result<()> {
        let client = self.pool.get().await?;
        // Scope the delete to this handle's namespace so one tenant can't forget
        // another's memory by guessing an id.
        client
            .execute(
                "DELETE FROM memories
                 WHERE id = $1
                   AND organization_id = $2
                   AND user_id IS NOT DISTINCT FROM $3",
                &[&id, &self.organization_id, &self.user_id],
            )
            .await?;
        Ok(())
    }

    /// Drive an async future to completion from a *synchronous* trait method.
    ///
    /// Identical bridge to [`PgKnowledgeBase::run_blocking`](crate::PgKnowledgeBase):
    /// spawn onto the captured runtime so its I/O makes progress on that
    /// runtime's reactor, then block on the `JoinHandle` from a throwaway OS
    /// thread running a tiny current-thread runtime — never `Handle::block_on` on
    /// a worker thread (which panics "Cannot start a runtime from within a
    /// runtime").
    fn run_blocking<F, T>(&self, fut: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        let join = self.handle.spawn(fut);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = (|| -> Result<T> {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                let joined = rt.block_on(join);
                joined.map_err(|e| anyhow!("memory task panicked or was cancelled: {e}"))?
            })();
            let _ = tx.send(result);
        });
        rx.recv()
            .map_err(|e| anyhow!("memory task channel closed: {e}"))?
    }
}

impl Memory for PgMemory {
    fn store(&self, entry: MemoryEntry) -> Result<()> {
        let this = self.clone();
        self.run_blocking(async move { this.store_async(entry).await })
    }

    fn recall(&self, query: &str, limit: usize) -> Result<Vec<MemoryEntry>> {
        let this = self.clone();
        let query = query.to_string();
        self.run_blocking(async move { this.recall_async(&query, limit).await })
    }

    fn forget(&self, id: &str) -> Result<()> {
        let this = self.clone();
        let id = id.to_string();
        self.run_blocking(async move { this.forget_async(&id).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_type_round_trips_through_text() {
        for mt in [
            MemoryType::ShortTerm,
            MemoryType::LongTerm,
            MemoryType::Entity,
            MemoryType::User,
            MemoryType::Feedback,
            MemoryType::Project,
            MemoryType::Reference,
        ] {
            let s = PgMemory::memory_type_to_str(mt).expect("to_str");
            // Stored form is the bare serde tag, no surrounding quotes.
            assert!(
                !s.contains('"'),
                "stored memory_type must be unquoted: {s:?}"
            );
            let parsed = PgMemory::memory_type_from_str(&s).expect("from_str");
            assert_eq!(parsed, mt);
        }
    }

    #[test]
    fn vector_literal_shape() {
        assert_eq!(PgMemory::vector_literal(&[0.5, -1.0, 2.0]), "[0.5,-1,2]");
        assert_eq!(PgMemory::vector_literal(&[]), "[]");
    }
}
