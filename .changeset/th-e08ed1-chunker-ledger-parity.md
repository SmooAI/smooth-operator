---
'@smooai/smooth-operator': patch
---

.NET ingestion parity: paragraph-aware chunker + content-hash IngestLedger

Bring the .NET `Chunker` to parity with the Rust ingestion chunker — ~500-char
paragraph-aware chunks (blank-line units, oversized paragraphs hard-split on word
boundaries, greedy packing) with 64-char whole-word trailing overlap and stable
`{documentId}#{index}` chunk ids (replacing the old whitespace-break 1200/150
sliding-window splitter). Add a new `IngestLedger` with FNV-1a content-hash
idempotency (byte-identical to Rust's `content_hash`) so re-ingesting identical
content is a no-op while changed content is reprocessed; wire it through
`IngestPipeline` (skips unchanged documents, dedupes identical chunks).
