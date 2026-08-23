---
"@smooai/smooth-operator": patch
---

test: fence the ingest→ACL chain at the pipeline seam, not just the GitHub connector

The guarantee that a connector's document ACL survives ingestion — `RawDocument::acl`
→ chunk → structured `DocAcl` → `AclKnowledgeStore` side table → `AclReader` — was
asserted end to end in exactly one place: `github_connector.rs`. That test is real, but
it is a *connector* test. Delete or rewrite the GitHub connector and the ingest half of
G3 loses its only fence, silently, with the ingestion contract test still green.

`ingestion_contract.rs::ingested_acls_gate_retrieval_for_every_connector` asserts the
same chain at the pipeline seam over a `MockConnector`, so it holds for every connector
present and future: a document ingested for `group-eng` is readable by a principal
carrying that group and returns **nothing** for `group-fin` or for anonymous, while a
document ingested with no ACL stays org-public. Each negative assertion is paired with
the entitled-principal positive control on the same query, so a pipeline that stored
nothing cannot satisfy "nothing leaked" vacuously — the failure mode this repo has
shipped before.

Verified red before green: with `DocAcl::for_groups(...).attach_to(document)` reverted
in `pipeline.rs`, the new test fails with `group-fin must not read the group-eng doc,
got 1 hits` — the exact G3 cross-user leak — while the pre-existing contract test stays
green, which is what made the gap invisible.

No production behavior changes. `docs/Planning/Feature Gaps.md` §G1/§G2/§G9 are updated
to record what actually shipped (the `Connector` seam, `MockConnector`, and the file /
web / github connectors landed some time ago and were never marked), the `pull` →
`Vec<RawDocument>` deviation from the planned `Stream<Document>` and why it should be
re-shaped before the SaaS connectors rather than after, and what remains: the connector
long tail, format extraction, and the nightly job that would actually run the gated
`external` tier.
