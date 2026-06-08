//! Single-table key construction (see `docs/STORAGE.md`).
//!
//! One DynamoDB table, overloaded `PK`/`SK` per entity, with two overloaded
//! GSIs for the non-primary access patterns:
//!
//! | Entity        | PK                  | SK                          | GSI1PK / GSI1SK                       |
//! | ------------- | ------------------- | --------------------------- | ------------------------------------- |
//! | Conversation  | `ORG#<org>`         | `CONV#<convId>`             | `IDEM#<org>#<idemKey>` / `CONV#<id>`  |
//! | Participant   | `CONV#<convId>`     | `PART#<partId>`             | `EXTERNAL#<convId>#<extId>` / `PART#` |
//! | Message       | `CONV#<convId>`     | `MSG#<zero-padded seq>#<id>`| —                                     |
//! | Session       | `CONV#<convId>`     | `SESS#<sessionId>`          | `SESSION#<sessionId>` / `SESS#<id>`  |
//! | Checkpoint    | `CKPT#<agentId>`    | `<zero-padded iteration>#<id>` | —                                  |
//!
//! `GSI1` serves three direct-lookup patterns (conversation-by-idempotency,
//! participant-by-external-id, session-by-id) via overloaded partition keys —
//! textbook single-table overloading. A standalone `get_conversation(id)`
//! (no org in hand) also uses a `CONV#<id>` GSI1 entry.
//!
//! Sequence numbers and iterations are zero-padded to a fixed width so DynamoDB's
//! lexicographic SK ordering matches numeric ordering (the classic sortable-key
//! pattern). 20 digits covers the full `u64` range.

/// Width for zero-padded numeric sort-key components (covers `u64::MAX`).
pub const SEQ_WIDTH: usize = 20;

/// Attribute names for the table and its GSIs. Centralized so the schema
/// definition and every query agree.
pub mod attr {
    pub const PK: &str = "pk";
    pub const SK: &str = "sk";
    pub const GSI1PK: &str = "gsi1pk";
    pub const GSI1SK: &str = "gsi1sk";
    /// Stored JSON document body (the serialized domain struct).
    pub const BODY: &str = "body";
    /// Entity discriminator (for debugging / scans).
    pub const ENTITY: &str = "entity";
    /// Per-conversation monotonic message sequence (number).
    pub const SEQ: &str = "seq";
    /// Knowledge: stored embedding (list of numbers) for the brute-force path.
    pub const EMBEDDING: &str = "embedding";

    // Indexing-run attributes. `IndexingRun` is not (de)serializable (the
    // ingestion crate's contract is intentionally untouched), so a run is stored
    // as discrete attributes — exactly as the Postgres adapter persists it
    // per-column.
    /// Indexing run id (uuid v4).
    pub const IX_ID: &str = "ixId";
    /// Indexing run connector name.
    pub const IX_CONNECTOR: &str = "ixConnector";
    /// Indexing run status (`running` / `succeeded` / `failed`).
    pub const IX_STATUS: &str = "ixStatus";
    /// Indexing run start time (RFC 3339).
    pub const IX_STARTED_AT: &str = "ixStartedAt";
    /// Indexing run finish time (RFC 3339; absent while running).
    pub const IX_FINISHED_AT: &str = "ixFinishedAt";
    /// Documents the connector returned this run.
    pub const IX_DOCS_SEEN: &str = "ixDocumentsSeen";
    /// Chunks newly embedded + stored this run.
    pub const IX_CHUNKS: &str = "ixChunksIndexed";
    /// Documents skipped as unchanged this run.
    pub const IX_DOCS_SKIPPED: &str = "ixDocumentsSkipped";
    /// New high-water cursor (RFC 3339; absent on failure / no-op runs).
    pub const IX_CURSOR: &str = "ixCursor";
    /// Failure message (absent unless status is `failed`).
    pub const IX_ERROR: &str = "ixError";
}

/// GSI1 index name.
pub const GSI1: &str = "gsi1";

/// Zero-pad a `u64` to [`SEQ_WIDTH`] so lexicographic == numeric ordering.
#[must_use]
pub fn pad(n: u64) -> String {
    format!("{n:0SEQ_WIDTH$}", SEQ_WIDTH = SEQ_WIDTH)
}

// ---- conversations ---------------------------------------------------------

#[must_use]
pub fn conv_pk(org: &str) -> String {
    format!("ORG#{org}")
}

#[must_use]
pub fn conv_sk(conv_id: &str) -> String {
    format!("CONV#{conv_id}")
}

/// Sort key of the per-conversation idempotency-claim item (lives under the
/// org partition). Guards conversation-create idempotency on `idempotencyKey`.
#[must_use]
pub fn conv_idem_sk(idempotency_key: &str) -> String {
    format!("IDEM#{idempotency_key}")
}

/// GSI1 partition for resolving a conversation by id alone (no org in hand).
#[must_use]
pub fn conv_id_gsi1pk(conv_id: &str) -> String {
    format!("CONV#{conv_id}")
}

// ---- participants ----------------------------------------------------------

#[must_use]
pub fn part_pk(conv_id: &str) -> String {
    format!("CONV#{conv_id}")
}

#[must_use]
pub fn part_sk(part_id: &str) -> String {
    format!("PART#{part_id}")
}

/// SK prefix for `begins_with` listing of a conversation's participants.
pub const PART_SK_PREFIX: &str = "PART#";

/// GSI1 partition for resolving a participant by `(conversationId, externalId)`.
#[must_use]
pub fn part_external_gsi1pk(conv_id: &str, external_id: &str) -> String {
    format!("EXTERNAL#{conv_id}#{external_id}")
}

// ---- messages --------------------------------------------------------------

#[must_use]
pub fn msg_pk(conv_id: &str) -> String {
    format!("CONV#{conv_id}")
}

/// Message SK: `MSG#<zero-padded seq>#<msgId>` — seq-ordered, id-disambiguated.
#[must_use]
pub fn msg_sk(seq: u64, msg_id: &str) -> String {
    format!("MSG#{}#{}", pad(seq), msg_id)
}

/// SK prefix for `begins_with` paging of a conversation's messages.
pub const MSG_SK_PREFIX: &str = "MSG#";

/// GSI1 partition for resolving a message by id alone.
#[must_use]
pub fn msg_id_gsi1pk(msg_id: &str) -> String {
    format!("MSG#{msg_id}")
}

// ---- sessions --------------------------------------------------------------

#[must_use]
pub fn sess_pk(conv_id: &str) -> String {
    format!("CONV#{conv_id}")
}

#[must_use]
pub fn sess_sk(session_id: &str) -> String {
    format!("SESS#{session_id}")
}

/// SK prefix for `begins_with` listing of a conversation's sessions.
pub const SESS_SK_PREFIX: &str = "SESS#";

/// GSI1 partition for resolving a session by id alone.
#[must_use]
pub fn sess_gsi1pk(session_id: &str) -> String {
    format!("SESSION#{session_id}")
}

// ---- per-conversation message sequence counter -----------------------------

/// PK/SK of the atomic counter item that hands out monotonic message seqs for a
/// conversation. A single `UpdateItem ADD seq :1` per append gives a gap-free
/// total order without a scan.
#[must_use]
pub fn seq_counter_pk(conv_id: &str) -> String {
    format!("CONV#{conv_id}")
}

pub const SEQ_COUNTER_SK: &str = "SEQ#";

// ---- checkpoints -----------------------------------------------------------

#[must_use]
pub fn ckpt_pk(agent_id: &str) -> String {
    format!("CKPT#{agent_id}")
}

/// Checkpoint SK: `<zero-padded iteration>#<checkpointId>`. Sortable by
/// iteration, id-disambiguated so two checkpoints at the same iteration don't
/// collide. `load_latest` = `Query(Limit=1, ScanIndexForward=false)`.
#[must_use]
pub fn ckpt_sk(iteration: u32, checkpoint_id: &str) -> String {
    format!("{}#{}", pad(u64::from(iteration)), checkpoint_id)
}

/// GSI1 partition for resolving a checkpoint by id alone (the `load(id)` path).
#[must_use]
pub fn ckpt_id_gsi1pk(checkpoint_id: &str) -> String {
    format!("CKPTID#{checkpoint_id}")
}

// ---- admin: connector configs ----------------------------------------------

/// Connector configs are partitioned per org so `list(org)` is a single query
/// and one org can never see another's connectors.
#[must_use]
pub fn connector_pk(org: &str) -> String {
    format!("ORG#{org}")
}

#[must_use]
pub fn connector_sk(id: &str) -> String {
    format!("CONNECTOR#{id}")
}

/// SK prefix for `begins_with` listing of an org's connector configs.
pub const CONNECTOR_SK_PREFIX: &str = "CONNECTOR#";

// ---- admin: per-org agent settings -----------------------------------------

/// Agent settings live under the org partition at a fixed singleton SK — one
/// settings row per org, read/written by `get`/`put`.
#[must_use]
pub fn settings_pk(org: &str) -> String {
    format!("ORG#{org}")
}

pub const SETTINGS_SK: &str = "SETTINGS#";

// ---- admin: indexing runs --------------------------------------------------

/// Indexing runs are partitioned per connector so `list_runs(name)` is a single
/// query and `latest_cursor(name)` scans only that connector's runs.
#[must_use]
pub fn indexing_pk(connector_name: &str) -> String {
    format!("IXCONN#{connector_name}")
}

/// Indexing-run SK: `<zero-padded started_at millis>#<runId>` — sortable by
/// start time (lexicographic == chronological), id-disambiguated so two runs
/// that start in the same millisecond don't collide. `list_runs` queries this
/// partition ascending (oldest-first, matching the in-memory contract).
#[must_use]
pub fn indexing_sk(started_at_millis: i64, run_id: &str) -> String {
    // Offset into the non-negative u64 range so pre-epoch timestamps (negative
    // millis) still pad to a fixed width and sort correctly. i64::MIN..i64::MAX
    // maps monotonically onto 0..u64::MAX.
    let ordered = (started_at_millis as i128 - i64::MIN as i128) as u128;
    format!("{ordered:0SEQ_WIDTH$}#{run_id}", SEQ_WIDTH = SEQ_WIDTH)
}

// ---- knowledge -------------------------------------------------------------

/// Knowledge items are partitioned per org so a brute-force scan only touches
/// one org's corpus.
#[must_use]
pub fn knowledge_pk(org: &str) -> String {
    format!("KNOW#{org}")
}

#[must_use]
pub fn knowledge_sk(doc_id: &str) -> String {
    format!("DOC#{doc_id}")
}

/// SK prefix for `begins_with` over a knowledge partition.
pub const KNOWLEDGE_SK_PREFIX: &str = "DOC#";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padding_preserves_numeric_order_lexically() {
        assert!(pad(2) < pad(10));
        assert!(pad(9) < pad(100));
        assert!(pad(u64::MAX - 1) < pad(u64::MAX));
        assert_eq!(pad(0).len(), SEQ_WIDTH);
        assert_eq!(pad(u64::MAX).len(), SEQ_WIDTH);
    }

    #[test]
    fn message_sk_is_seq_sortable() {
        // Lexicographic SK order must equal seq order regardless of id.
        let a = msg_sk(2, "zzz");
        let b = msg_sk(10, "aaa");
        assert!(a < b, "seq 2 must sort before seq 10");
    }

    #[test]
    fn checkpoint_sk_is_iteration_sortable() {
        assert!(ckpt_sk(1, "z") < ckpt_sk(2, "a"));
        assert!(ckpt_sk(9, "z") < ckpt_sk(10, "a"));
    }
}
