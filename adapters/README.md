# `adapters/` — storage backends

One `StorageAdapter` trait, two production implementations. Application and agent code never name a database.

- `postgres/` — k8s / self-host path. Conversation tables + `PostgresCheckpointStore` (from smooth-operator-core) + `pgvector`/`tsvector` knowledge. Mirrors the smooai monorepo schema.
- `dynamodb/` — AWS serverless path. Raw `aws-sdk-dynamodb` single-table for conversation/participant/message/session/checkpoint + **Amazon S3 Vectors** for knowledge embeddings.

See [`../docs/STORAGE.md`](../docs/STORAGE.md) for the trait surface, the single-table key design, and why knowledge vectors go to S3 Vectors rather than raw DynamoDB.
