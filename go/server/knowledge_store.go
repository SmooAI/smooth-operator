package server

// Durable, vector-searched knowledge storage for the Go server — the Go analog of
// the C# PostgresKnowledgeBase / PostgresAclKnowledgeStore (dotnet/server/postgres)
// and the Rust knowledge_vectors adapter (rust/adapters/postgres/src/knowledge.rs).
// Documents are embedded and stored as pgvector `vector` rows; retrieval embeds the
// query and ranks by cosine distance (`<=>`). The ACL variant persists each doc's
// entitlement (public, or restricted to groups) and filters by the caller's groups
// IN SQL, so a restricted document is never even fetched — the leak boundary that
// survives the ingest→serve process boundary.
//
// Unlike the OLTP/admin tables in postgres_store.go (created verbatim by
// NewPostgresStore from the shared Rust schema), these tables are created by the
// knowledge store's own constructor: the `vector(N)` column width is a function of
// the embedder's dimension, and pgvector is an optional extension the session-store
// path must not require. The C# store makes the same split for the same reasons.
//
// The engine's grounding contract (core.Knowledge) is a weaker, id-less, lexical
// interface (Phase-0 parity). This store carries the richer id-keyed, ACL-aware
// contract of the reference servers; Grounding() bridges it down to core.Knowledge
// so the store can actually ground the agent.

import (
	"context"
	"fmt"
	"math"
	"strconv"
	"strings"
	"unicode"

	core "github.com/SmooAI/smooth-operator-core/go/core"
	"github.com/jackc/pgx/v5/pgxpool"
)

// Embedder turns text into an embedding vector for similarity search. Mirrors the
// Rust engine's Embedder trait and the C# IEmbedder: a real gateway embedder in
// production, a deterministic one for tests / offline use.
type Embedder interface {
	// Dimensions is the vector width; the `vector(N)` column is sized to it.
	Dimensions() int
	Embed(ctx context.Context, text string) ([]float32, error)
}

// DeterministicEmbedder is a network-free embedder: hashed bag-of-words (FNV-1a)
// into a fixed-dimension, L2-normalized vector. Same text → same vector, and texts
// sharing tokens are close in cosine space. The Go analog of the Rust/C#
// DeterministicEmbedder; ideal for tests and small in-process corpora.
type DeterministicEmbedder struct{ dim int }

// NewDeterministicEmbedder returns an embedder of the given dimension (default 256
// when dim <= 0), matching the C# DeterministicEmbedder(256) used by its tests.
func NewDeterministicEmbedder(dim int) DeterministicEmbedder {
	if dim <= 0 {
		dim = 256
	}
	return DeterministicEmbedder{dim: dim}
}

// Dimensions is the vector width.
func (e DeterministicEmbedder) Dimensions() int { return e.dim }

// Embed hashes each token into a slot and L2-normalizes. Never errors (kept in the
// signature so a live embedder is a drop-in).
func (e DeterministicEmbedder) Embed(_ context.Context, text string) ([]float32, error) {
	vec := make([]float32, e.dim)
	for _, tok := range embedTokenize(text) {
		vec[fnv1a(tok)%uint32(e.dim)] += 1
	}
	var sumSq float64
	for _, v := range vec {
		sumSq += float64(v) * float64(v)
	}
	if sumSq > 0 {
		norm := float32(math.Sqrt(sumSq))
		for i := range vec {
			vec[i] /= norm
		}
	}
	return vec, nil
}

// embedTokenize lowercases, splits on whitespace, strips non-alphanumeric runes, and
// keeps tokens longer than two chars — identical to the C# DeterministicEmbedder so
// the two produce comparable vectors.
func embedTokenize(text string) []string {
	var out []string
	for _, raw := range strings.Fields(strings.ToLower(text)) {
		var b strings.Builder
		for _, r := range raw {
			if unicode.IsLetter(r) || unicode.IsDigit(r) {
				b.WriteRune(r)
			}
		}
		if tok := b.String(); len(tok) > 2 {
			out = append(out, tok)
		}
	}
	return out
}

func fnv1a(s string) uint32 {
	var h uint32 = 2166136261
	for _, c := range s {
		h ^= uint32(c)
		h *= 16777619
	}
	return h
}

// vectorLiteral formats an embedding as a pgvector text literal `[0.1,0.2,...]`.
// Passed as a text param and cast `::vector` in SQL — the same approach as the Rust
// adapter, so no pgvector-specific pgx type registration is needed.
func vectorLiteral(v []float32) string {
	var b strings.Builder
	b.Grow(len(v)*8 + 2)
	b.WriteByte('[')
	for i, x := range v {
		if i > 0 {
			b.WriteByte(',')
		}
		b.WriteString(strconv.FormatFloat(float64(x), 'f', -1, 32))
	}
	b.WriteByte(']')
	return b.String()
}

// KnowledgeDocument is a document to ingest, keyed by Id (re-ingesting the same id
// upserts in place). Mirrors the C# KnowledgeDocument.
type KnowledgeDocument struct {
	ID      string
	Content string
	Source  string
}

// KnowledgeResult is one retrieved document with its cosine-similarity score.
// Mirrors the C# KnowledgeResult.
type KnowledgeResult struct {
	DocumentID string
	Chunk      string
	Score      float64
	Source     string
}

// DocumentAcl is a document's access control: public, or restricted to Groups.
// Mirrors the C# DocumentAcl.
type DocumentAcl struct {
	Public bool
	Groups []string
}

// PublicAcl is the ACL for a world-readable document.
var PublicAcl = DocumentAcl{Public: true}

// AclForGroups restricts a document to the given entitlement groups.
func AclForGroups(groups ...string) DocumentAcl {
	return DocumentAcl{Public: false, Groups: groups}
}

// ── vector knowledge base ────────────────────────────────────────────────────

// PostgresKnowledgeBase is a durable, vector-searched knowledge store backed by
// Postgres + pgvector. The Go analog of the C# PostgresKnowledgeBase.
type PostgresKnowledgeBase struct {
	pool     *pgxpool.Pool
	embedder Embedder
}

// NewPostgresKnowledgeBase connects, ensures the `vector` extension, and creates the
// knowledge_documents table sized to the embedder's dimension (all idempotent).
func NewPostgresKnowledgeBase(ctx context.Context, connString string, embedder Embedder) (*PostgresKnowledgeBase, error) {
	pool, err := pgxpool.New(ctx, connString)
	if err != nil {
		return nil, fmt.Errorf("knowledge: connect: %w", err)
	}
	// Extension + table in one simple-protocol Exec (no args). pgvector is created
	// here rather than in postgresSchema so the session-store path never requires it.
	schema := fmt.Sprintf(`
CREATE EXTENSION IF NOT EXISTS vector;
CREATE TABLE IF NOT EXISTS knowledge_documents (
    id        TEXT PRIMARY KEY,
    content   TEXT NOT NULL,
    source    TEXT NOT NULL,
    embedding vector(%d)
);`, embedder.Dimensions())
	if _, err := pool.Exec(ctx, schema); err != nil {
		pool.Close()
		return nil, fmt.Errorf("knowledge: apply schema: %w", err)
	}
	return &PostgresKnowledgeBase{pool: pool, embedder: embedder}, nil
}

// Close releases the connection pool.
func (k *PostgresKnowledgeBase) Close() { k.pool.Close() }

// Ingest embeds the document and upserts it by id (idempotent).
func (k *PostgresKnowledgeBase) Ingest(ctx context.Context, doc KnowledgeDocument) error {
	emb, err := k.embedder.Embed(ctx, doc.Content)
	if err != nil {
		return fmt.Errorf("knowledge: embed: %w", err)
	}
	_, err = k.pool.Exec(ctx, `
INSERT INTO knowledge_documents (id, content, source, embedding)
VALUES ($1, $2, $3, $4::vector)
ON CONFLICT (id) DO UPDATE SET content = $2, source = $3, embedding = $4::vector`,
		doc.ID, doc.Content, doc.Source, vectorLiteral(emb))
	if err != nil {
		return fmt.Errorf("knowledge: ingest: %w", err)
	}
	return nil
}

// Query embeds the query and returns the top-`limit` documents by cosine similarity.
func (k *PostgresKnowledgeBase) Query(ctx context.Context, query string, limit int) ([]KnowledgeResult, error) {
	emb, err := k.embedder.Embed(ctx, query)
	if err != nil {
		return nil, fmt.Errorf("knowledge: embed query: %w", err)
	}
	// `<=>` is cosine distance; 1 - distance is cosine similarity (the score).
	rows, err := k.pool.Query(ctx, `
SELECT id, content, source, 1 - (embedding <=> $1::vector) AS score
FROM knowledge_documents
WHERE embedding IS NOT NULL
ORDER BY embedding <=> $1::vector
LIMIT $2`, vectorLiteral(emb), limit)
	if err != nil {
		return nil, fmt.Errorf("knowledge: query: %w", err)
	}
	defer rows.Close()
	return scanKnowledgeResults(rows)
}

// Grounding adapts this store to the engine's core.Knowledge contract (id-less,
// error-less lexical grounding) so it can ground the agent.
func (k *PostgresKnowledgeBase) Grounding() core.Knowledge {
	return groundingAdapter(func(query string, topK int) ([]KnowledgeResult, error) {
		return k.Query(context.Background(), query, topK)
	})
}

// ── ACL-aware vector knowledge store ─────────────────────────────────────────

// PostgresAclKnowledgeStore is a durable, ACL-aware, vector-searched knowledge store.
// Each document carries an ACL persisted in acl_public / acl_groups; retrieval filters
// by the caller's groups IN SQL before ranking. The Go analog of the C#
// PostgresAclKnowledgeStore.
type PostgresAclKnowledgeStore struct {
	pool     *pgxpool.Pool
	embedder Embedder
}

// NewPostgresAclKnowledgeStore connects, ensures pgvector, and creates the
// acl_knowledge_documents table sized to the embedder's dimension (all idempotent).
func NewPostgresAclKnowledgeStore(ctx context.Context, connString string, embedder Embedder) (*PostgresAclKnowledgeStore, error) {
	pool, err := pgxpool.New(ctx, connString)
	if err != nil {
		return nil, fmt.Errorf("acl-knowledge: connect: %w", err)
	}
	schema := fmt.Sprintf(`
CREATE EXTENSION IF NOT EXISTS vector;
CREATE TABLE IF NOT EXISTS acl_knowledge_documents (
    id         TEXT PRIMARY KEY,
    content    TEXT NOT NULL,
    source     TEXT NOT NULL,
    embedding  vector(%d),
    acl_public BOOLEAN NOT NULL DEFAULT true,
    acl_groups TEXT[]  NOT NULL DEFAULT ARRAY[]::TEXT[]
);`, embedder.Dimensions())
	if _, err := pool.Exec(ctx, schema); err != nil {
		pool.Close()
		return nil, fmt.Errorf("acl-knowledge: apply schema: %w", err)
	}
	return &PostgresAclKnowledgeStore{pool: pool, embedder: embedder}, nil
}

// Close releases the connection pool.
func (s *PostgresAclKnowledgeStore) Close() { s.pool.Close() }

// Ingest embeds the document and upserts it with its ACL (idempotent by id).
func (s *PostgresAclKnowledgeStore) Ingest(ctx context.Context, doc KnowledgeDocument, acl DocumentAcl) error {
	emb, err := s.embedder.Embed(ctx, doc.Content)
	if err != nil {
		return fmt.Errorf("acl-knowledge: embed: %w", err)
	}
	groups := acl.Groups
	if groups == nil {
		groups = []string{}
	}
	_, err = s.pool.Exec(ctx, `
INSERT INTO acl_knowledge_documents (id, content, source, embedding, acl_public, acl_groups)
VALUES ($1, $2, $3, $4::vector, $5, $6)
ON CONFLICT (id) DO UPDATE SET
    content = $2, source = $3, embedding = $4::vector,
    acl_public = $5, acl_groups = $6`,
		doc.ID, doc.Content, doc.Source, vectorLiteral(emb), acl.Public, groups)
	if err != nil {
		return fmt.Errorf("acl-knowledge: ingest: %w", err)
	}
	return nil
}

// AclScopedView is a read-only view of the store scoped to one caller's access.
type AclScopedView struct {
	store  *PostgresAclKnowledgeStore
	groups []string
}

// ForAccess returns a view whose queries enforce the caller's group entitlements.
// Anonymous (no groups) sees public documents only — fail-closed.
func (s *PostgresAclKnowledgeStore) ForAccess(access AccessContext) *AclScopedView {
	return &AclScopedView{store: s, groups: access.Groups()}
}

// Query embeds the query and returns the top-`limit` documents the caller may see:
// a doc is visible if it is public, or its groups overlap the caller's (`&&` is
// array-overlap). The ACL filter is applied IN SQL before ranking.
func (v *AclScopedView) Query(ctx context.Context, query string, limit int) ([]KnowledgeResult, error) {
	emb, err := v.store.embedder.Embed(ctx, query)
	if err != nil {
		return nil, fmt.Errorf("acl-knowledge: embed query: %w", err)
	}
	groups := v.groups
	if groups == nil {
		groups = []string{}
	}
	rows, err := v.store.pool.Query(ctx, `
SELECT id, content, source, 1 - (embedding <=> $1::vector) AS score
FROM acl_knowledge_documents
WHERE embedding IS NOT NULL AND (acl_public OR acl_groups && $2)
ORDER BY embedding <=> $1::vector
LIMIT $3`, vectorLiteral(emb), groups, limit)
	if err != nil {
		return nil, fmt.Errorf("acl-knowledge: query: %w", err)
	}
	defer rows.Close()
	return scanKnowledgeResults(rows)
}

// Grounding adapts this scoped view to the engine's core.Knowledge contract.
func (v *AclScopedView) Grounding() core.Knowledge {
	return groundingAdapter(func(query string, topK int) ([]KnowledgeResult, error) {
		return v.Query(context.Background(), query, topK)
	})
}

// ── shared helpers ───────────────────────────────────────────────────────────

// pgxRows is the subset of pgx.Rows scanKnowledgeResults needs (kept small so both
// query paths share one row-mapper).
type pgxRows interface {
	Next() bool
	Scan(dest ...any) error
	Err() error
}

func scanKnowledgeResults(rows pgxRows) ([]KnowledgeResult, error) {
	var out []KnowledgeResult
	for rows.Next() {
		var r KnowledgeResult
		if err := rows.Scan(&r.DocumentID, &r.Chunk, &r.Source, &r.Score); err != nil {
			return nil, fmt.Errorf("knowledge: scan: %w", err)
		}
		out = append(out, r)
	}
	if err := rows.Err(); err != nil {
		return nil, fmt.Errorf("knowledge: rows: %w", err)
	}
	return out, nil
}

// groundingAdapter maps the rich KnowledgeResult query down to core.Knowledge
// (Content/Source/Score; ids dropped, errors → no hits — the engine grounds on
// whatever it gets or honestly declines).
type groundingAdapter func(query string, topK int) ([]KnowledgeResult, error)

func (g groundingAdapter) Query(query string, topK int) []core.KnowledgeHit {
	results, err := g(query, topK)
	if err != nil {
		return nil
	}
	hits := make([]core.KnowledgeHit, 0, len(results))
	for _, r := range results {
		hits = append(hits, core.KnowledgeHit{Content: r.Chunk, Source: r.Source, Score: r.Score})
	}
	return hits
}
