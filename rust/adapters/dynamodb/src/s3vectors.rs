//! Amazon S3 Vectors knowledge backend (the production AWS dense-retrieval path).
//!
//! **Crate situation:** `aws-sdk-s3vectors` **exists** on crates.io (v1.x — S3
//! Vectors went GA 2025-12-02), so this is a **real implementation**, not a stub
//! or a `todo!()`. It is gated behind the `s3-vectors` cargo feature so the
//! default build (and the conformance test, which uses the brute-force DynamoDB
//! path against DynamoDB-Local) needs neither the extra dependency nor live AWS.
//!
//! Design (per `docs/STORAGE.md`): DynamoDB owns the OLTP doc metadata, S3
//! Vectors owns dense retrieval. The knowledge slice writes the chunk + metadata
//! to DynamoDB and the embedding to an S3 Vectors **index per org**, keyed by the
//! same document id. `query` embeds the query and calls `query_vectors`
//! (`top_k`, `return_metadata`, `return_distance`), turning S3-Vectors distance
//! into a similarity score.
//!
//! This path is **not** exercised by the conformance test (which runs entirely
//! against DynamoDB-Local with no S3 Vectors endpoint). It compiles under
//! `--features s3-vectors` and targets a live AWS account; a local-emulator
//! integration test is a follow-up once a S3-Vectors test double exists.

use anyhow::{anyhow, Result};
use aws_sdk_s3vectors::types::{PutInputVector, VectorData};
use aws_sdk_s3vectors::Client as S3VectorsClient;
use aws_smithy_types::Document as SmithyDocument;

use smooth_operator::{Document, KnowledgeResult};

/// Configuration for the S3 Vectors knowledge backend.
#[derive(Debug, Clone)]
pub struct S3VectorsConfig {
    /// The S3 vector bucket holding the per-org indexes.
    pub vector_bucket_name: String,
    /// Prefix for per-org index names; the org id is appended
    /// (`{index_prefix}-{org}`).
    pub index_prefix: String,
}

impl S3VectorsConfig {
    #[must_use]
    pub fn new(vector_bucket_name: impl Into<String>, index_prefix: impl Into<String>) -> Self {
        Self {
            vector_bucket_name: vector_bucket_name.into(),
            index_prefix: index_prefix.into(),
        }
    }

    fn index_name(&self, org: &str) -> String {
        format!("{}-{}", self.index_prefix, org)
    }
}

/// A live S3 Vectors store. Constructed lazily by [`DynamoKnowledgeBase`] when
/// the `S3Vectors` backend is selected.
pub struct S3VectorsStore {
    config: S3VectorsConfig,
    client: tokio::sync::OnceCell<S3VectorsClient>,
}

impl S3VectorsStore {
    #[must_use]
    pub fn new(config: S3VectorsConfig) -> Self {
        Self {
            config,
            client: tokio::sync::OnceCell::new(),
        }
    }

    /// Build (once) the S3 Vectors client from the ambient AWS config.
    async fn client(&self) -> &S3VectorsClient {
        self.client
            .get_or_init(|| async {
                let conf = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
                S3VectorsClient::new(&conf)
            })
            .await
    }

    /// Upsert one document's embedding into the org's index, with the document
    /// id / source / content carried as vector metadata for retrieval.
    pub async fn upsert(&self, org: &str, doc: &Document, embedding: &[f32]) -> Result<()> {
        let metadata = SmithyDocument::Object(
            [
                (
                    "documentId".to_string(),
                    SmithyDocument::String(doc.id.clone()),
                ),
                (
                    "source".to_string(),
                    SmithyDocument::String(doc.source.clone()),
                ),
                (
                    "content".to_string(),
                    SmithyDocument::String(doc.content.clone()),
                ),
            ]
            .into_iter()
            .collect(),
        );

        let vector = PutInputVector::builder()
            .key(doc.id.clone())
            .data(VectorData::Float32(embedding.to_vec()))
            .metadata(metadata)
            .build()
            .map_err(|e| anyhow!("building s3 vectors PutInputVector: {e}"))?;

        self.client()
            .await
            .put_vectors()
            .vector_bucket_name(&self.config.vector_bucket_name)
            .index_name(self.config.index_name(org))
            .vectors(vector)
            .send()
            .await
            .map_err(|e| anyhow!("s3 vectors put_vectors: {e}"))?;
        Ok(())
    }

    /// Query the org's index for the `limit` nearest vectors, returning results
    /// in the same shape as the brute-force path.
    pub async fn query(
        &self,
        org: &str,
        query_vec: &[f32],
        limit: usize,
    ) -> Result<Vec<KnowledgeResult>> {
        let top_k = i32::try_from(limit).unwrap_or(i32::MAX);
        let out = self
            .client()
            .await
            .query_vectors()
            .vector_bucket_name(&self.config.vector_bucket_name)
            .index_name(self.config.index_name(org))
            .query_vector(VectorData::Float32(query_vec.to_vec()))
            .top_k(top_k)
            .return_metadata(true)
            .return_distance(true)
            .send()
            .await
            .map_err(|e| anyhow!("s3 vectors query_vectors: {e}"))?;

        let mut results = Vec::new();
        for v in out.vectors() {
            let (document_id, source, content) = extract_meta(v.metadata.as_ref());
            // S3 Vectors returns cosine *distance* in [0, 2]; map to a similarity
            // score so higher == more relevant, matching the brute-force arm.
            let score = v.distance.map_or(0.0, |d| 1.0 - d);
            results.push(KnowledgeResult {
                document_id: document_id.unwrap_or_else(|| v.key.clone()),
                chunk: content.unwrap_or_default(),
                score,
                source: source.unwrap_or_default(),
            });
        }
        Ok(results)
    }
}

/// Pull `documentId` / `source` / `content` strings out of vector metadata.
fn extract_meta(meta: Option<&SmithyDocument>) -> (Option<String>, Option<String>, Option<String>) {
    let Some(SmithyDocument::Object(map)) = meta else {
        return (None, None, None);
    };
    let get = |k: &str| match map.get(k) {
        Some(SmithyDocument::String(s)) => Some(s.clone()),
        _ => None,
    };
    (get("documentId"), get("source"), get("content"))
}
