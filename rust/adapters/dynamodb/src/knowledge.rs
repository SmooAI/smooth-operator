//! DynamoDB knowledge slice — two retrieval backends behind one [`KnowledgeBase`].
//!
//! Per `docs/STORAGE.md`, DynamoDB has **no vector type and no ANN index**, so
//! the AWS path offers two configurations:
//!
//! 1. **Brute-force DynamoDB** ([`KnowledgeBackend::BruteForce`], the default and
//!    the one the conformance test exercises): the embedding is stored as a list
//!    of numbers on each knowledge item; `query` embeds the query and scans the
//!    org's partition computing cosine in-process. O(n) — fine for small corpora
//!    and local tests, exactly as the design doc calls out. No extra services.
//! 2. **S3 Vectors** ([`KnowledgeBackend::S3Vectors`], behind the `s3-vectors`
//!    cargo feature): Amazon S3 Vectors (GA 2025-12) native vector store +
//!    similarity query, one index per org. See [`s3vectors`] for the crate
//!    situation — the `aws-sdk-s3vectors` crate **does exist** (v1.x on
//!    crates.io), so this is a real implementation, not a stub.
//!
//! Both share the [`Embedder`] seam ([`DeterministicEmbedder`] by default) and
//! the same sync→async bridge as the checkpoint store: [`KnowledgeBase`] is a
//! **synchronous** smooth-operator trait, the SDK is async, so each call
//! `spawn`s onto a captured runtime [`Handle`] and blocks on the `JoinHandle`
//! from a throwaway OS thread — never `Handle::block_on` on a worker.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_dynamodb::Client;
use tokio::runtime::Handle;

use smooth_operator::{Document, KnowledgeBase, KnowledgeResult};

use crate::checkpoint::aws_err;
use crate::embedder::{cosine_similarity, Embedder, InputType};
use crate::keys::{self, attr};

/// Which dense-retrieval backend the knowledge slice uses.
#[derive(Debug, Clone)]
pub enum KnowledgeBackend {
    /// Embeddings stored on DynamoDB items, cosine computed in-process per query
    /// over the org's partition. Default; what the conformance test runs.
    BruteForce,
    /// Amazon S3 Vectors (one index per org). Requires the `s3-vectors` feature.
    #[cfg(feature = "s3-vectors")]
    S3Vectors(crate::s3vectors::S3VectorsConfig),
}

/// DynamoDB-backed knowledge base. Cheap to clone.
#[derive(Clone)]
pub struct DynamoKnowledgeBase {
    client: Client,
    table: String,
    embedder: Arc<dyn Embedder>,
    handle: Handle,
    /// Org partition for ingest/query. The engine is single-org per agent, so a
    /// fixed org keeps the brute-force scan scoped to one partition.
    organization_id: String,
    backend: KnowledgeBackend,
    #[cfg(feature = "s3-vectors")]
    s3vectors: Option<Arc<crate::s3vectors::S3VectorsStore>>,
}

impl DynamoKnowledgeBase {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        client: Client,
        table: impl Into<String>,
        embedder: Arc<dyn Embedder>,
        handle: Handle,
        organization_id: impl Into<String>,
        backend: KnowledgeBackend,
    ) -> Self {
        #[cfg(feature = "s3-vectors")]
        let s3vectors = match &backend {
            KnowledgeBackend::S3Vectors(cfg) => {
                Some(Arc::new(crate::s3vectors::S3VectorsStore::new(cfg.clone())))
            }
            KnowledgeBackend::BruteForce => None,
        };
        Self {
            client: client.clone(),
            table: table.into(),
            embedder,
            handle,
            organization_id: organization_id.into(),
            backend,
            #[cfg(feature = "s3-vectors")]
            s3vectors,
        }
    }

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
                joined.map_err(|e| anyhow!("knowledge task panicked or was cancelled: {e}"))?
            })();
            let _ = tx.send(result);
        });
        rx.recv()
            .map_err(|e| anyhow!("knowledge task channel closed: {e}"))?
    }

    async fn ingest_async(&self, doc: Document) -> Result<()> {
        let embedding = self
            .embedder
            .embed(std::slice::from_ref(&doc.content), InputType::Document)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("embedder returned no vector"))?;

        // Always persist the document + metadata in DynamoDB (the OLTP owner of
        // doc metadata, per STORAGE.md) — both backends store it here.
        let metadata = serde_json::to_string(&doc.metadata)?;
        let embedding_av = AttributeValue::L(
            embedding
                .iter()
                .map(|f| AttributeValue::N(f.to_string()))
                .collect(),
        );
        self.client
            .put_item()
            .table_name(&self.table)
            .item(
                attr::PK,
                AttributeValue::S(keys::knowledge_pk(&self.organization_id)),
            )
            .item(attr::SK, AttributeValue::S(keys::knowledge_sk(&doc.id)))
            .item(attr::ENTITY, AttributeValue::S("knowledge".to_string()))
            .item("documentId", AttributeValue::S(doc.id.clone()))
            .item("source", AttributeValue::S(doc.source.clone()))
            .item("content", AttributeValue::S(doc.content.clone()))
            .item("metadata", AttributeValue::S(metadata))
            .item(attr::EMBEDDING, embedding_av)
            .send()
            .await
            .map_err(|e| anyhow!("dynamodb put knowledge: {}", aws_err(e)))?;

        // S3 Vectors path additionally writes the embedding to its index.
        #[cfg(feature = "s3-vectors")]
        if let Some(store) = &self.s3vectors {
            store
                .upsert(&self.organization_id, &doc, &embedding)
                .await?;
        }

        Ok(())
    }

    async fn query_async(&self, query: String, limit: usize) -> Result<Vec<KnowledgeResult>> {
        let query_vec = self
            .embedder
            .embed(std::slice::from_ref(&query), InputType::Query)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("embedder returned no query vector"))?;

        match &self.backend {
            KnowledgeBackend::BruteForce => self.query_brute_force(&query_vec, limit).await,
            #[cfg(feature = "s3-vectors")]
            KnowledgeBackend::S3Vectors(_) => {
                let store = self
                    .s3vectors
                    .as_ref()
                    .ok_or_else(|| anyhow!("s3 vectors store not initialized"))?;
                store.query(&self.organization_id, &query_vec, limit).await
            }
        }
    }

    /// Scan the org's knowledge partition, score each item by cosine similarity
    /// to the query vector, return the top-`limit`. O(n) in corpus size.
    async fn query_brute_force(
        &self,
        query_vec: &[f32],
        limit: usize,
    ) -> Result<Vec<KnowledgeResult>> {
        struct Hit {
            document_id: String,
            source: String,
            content: String,
            score: f32,
        }
        let mut hits: Vec<Hit> = Vec::new();

        let mut last_key: Option<std::collections::HashMap<String, AttributeValue>> = None;
        loop {
            let mut req = self
                .client
                .query()
                .table_name(&self.table)
                .key_condition_expression("#pk = :pk AND begins_with(#sk, :skp)")
                .expression_attribute_names("#pk", attr::PK)
                .expression_attribute_names("#sk", attr::SK)
                .expression_attribute_values(
                    ":pk",
                    AttributeValue::S(keys::knowledge_pk(&self.organization_id)),
                )
                .expression_attribute_values(
                    ":skp",
                    AttributeValue::S(keys::KNOWLEDGE_SK_PREFIX.to_string()),
                );
            if let Some(start) = last_key.take() {
                req = req.set_exclusive_start_key(Some(start));
            }
            let out = req
                .send()
                .await
                .map_err(|e| anyhow!("dynamodb query knowledge: {}", aws_err(e)))?;

            for item in out.items() {
                let embedding = item
                    .get(attr::EMBEDDING)
                    .and_then(|v| v.as_l().ok())
                    .map(|list| {
                        list.iter()
                            .filter_map(|av| av.as_n().ok())
                            .filter_map(|s| s.parse::<f32>().ok())
                            .collect::<Vec<f32>>()
                    })
                    .unwrap_or_default();
                if embedding.is_empty() {
                    continue;
                }
                let score = cosine_similarity(query_vec, &embedding);
                let document_id = item
                    .get("documentId")
                    .and_then(|v| v.as_s().ok())
                    .cloned()
                    .unwrap_or_default();
                let source = item
                    .get("source")
                    .and_then(|v| v.as_s().ok())
                    .cloned()
                    .unwrap_or_default();
                let content = item
                    .get("content")
                    .and_then(|v| v.as_s().ok())
                    .cloned()
                    .unwrap_or_default();
                hits.push(Hit {
                    document_id,
                    source,
                    content,
                    score,
                });
            }

            match out.last_evaluated_key() {
                Some(k) if !k.is_empty() => last_key = Some(k.clone()),
                _ => break,
            }
        }

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

impl KnowledgeBase for DynamoKnowledgeBase {
    fn ingest(&self, doc: Document) -> Result<()> {
        let this = self.clone();
        self.run_blocking(async move { this.ingest_async(doc).await })
    }

    fn query(&self, query: &str, limit: usize) -> Result<Vec<KnowledgeResult>> {
        let this = self.clone();
        let query = query.to_string();
        self.run_blocking(async move { this.query_async(query, limit).await })
    }
}
