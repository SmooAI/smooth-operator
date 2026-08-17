package server

// Contract tests for the durable vector-knowledge stores, against a REAL
// Postgres+pgvector in a throwaway container (testcontainers). The Go sibling of the
// C# KnowledgeBaseContractTests / AclKnowledgeContractTests and the Rust knowledge
// conformance suites: ingest then retrieve the relevant document; ingest is
// idempotent by id; and the ACL leak boundary (anonymous → public-only, entitled →
// private, unentitled → no leak) holds when SQL-filtered in Postgres.
//
// Gated on Docker exactly like postgres_store_test.go: no daemon → every test SKIPS.
// A dedicated pgvector image is used (the alpine image the session tests use has no
// vector extension). Reuses within / dockerPingTimeout / containerUpTimeout from
// postgres_store_test.go (same package).

import (
	"context"
	"fmt"
	"os/exec"
	"strings"
	"sync"
	"testing"

	"github.com/testcontainers/testcontainers-go/modules/postgres"
)

// sharedVectorPostgres starts ONE pgvector container for the whole package and caches
// the DSN (or the failure, so a Docker-less machine pays for it once). Same shape as
// sharedPostgres but on an image that ships the `vector` extension.
var sharedVectorPostgres = sync.OnceValues(func() (string, error) {
	if err := within(dockerPingTimeout, func(ctx context.Context) error {
		return exec.CommandContext(ctx, "docker", "version", "--format", "{{.Server.Version}}").Run()
	}); err != nil {
		return "", fmt.Errorf("docker daemon not reachable: %w", err)
	}
	var dsn string
	err := within(containerUpTimeout, func(ctx context.Context) error {
		container, err := postgres.Run(ctx, "pgvector/pgvector:pg16", postgres.BasicWaitStrategies())
		if err != nil {
			return err
		}
		dsn, err = container.ConnectionString(ctx, "sslmode=disable")
		return err
	})
	return dsn, err
})

func vectorDSN(t *testing.T) string {
	t.Helper()
	dsn, err := sharedVectorPostgres()
	if err != nil {
		t.Skipf("SKIP: could not start pgvector container (Docker unavailable?): %v", err)
	}
	return dsn
}

// newKnowledgeBase returns a knowledge base on the shared container with an EMPTY
// table (truncated), so tests sharing the container don't see each other's rows.
func newKnowledgeBase(t *testing.T) *PostgresKnowledgeBase {
	t.Helper()
	kb, err := NewPostgresKnowledgeBase(t.Context(), vectorDSN(t), NewDeterministicEmbedder(256))
	if err != nil {
		t.Fatalf("NewPostgresKnowledgeBase: %v", err)
	}
	if _, err := kb.pool.Exec(t.Context(), "TRUNCATE knowledge_documents"); err != nil {
		t.Fatalf("truncate: %v", err)
	}
	t.Cleanup(kb.Close)
	return kb
}

func newAclKnowledgeStore(t *testing.T) *PostgresAclKnowledgeStore {
	t.Helper()
	store, err := NewPostgresAclKnowledgeStore(t.Context(), vectorDSN(t), NewDeterministicEmbedder(256))
	if err != nil {
		t.Fatalf("NewPostgresAclKnowledgeStore: %v", err)
	}
	if _, err := store.pool.Exec(t.Context(), "TRUNCATE acl_knowledge_documents"); err != nil {
		t.Fatalf("truncate: %v", err)
	}
	t.Cleanup(store.Close)
	return store
}

// ── KnowledgeBase contract ───────────────────────────────────────────────────

func TestKnowledgeIngestThenQueryRanksRelevantDocFirst(t *testing.T) {
	kb := newKnowledgeBase(t)
	ctx := t.Context()
	mustIngest(t, kb, "returns", "Our return window is 17 days from delivery.", "returns.md")
	mustIngest(t, kb, "shipping", "Standard shipping takes 5 to 7 business days.", "shipping.md")

	hits, err := kb.Query(ctx, "how long is the return window", 4)
	if err != nil {
		t.Fatalf("Query: %v", err)
	}
	if len(hits) == 0 {
		t.Fatal("expected hits, got none")
	}
	if hits[0].DocumentID != "returns" {
		t.Fatalf("expected top hit 'returns', got %q", hits[0].DocumentID)
	}
	if !strings.Contains(hits[0].Chunk, "17 days") {
		t.Fatalf("top hit chunk missing '17 days': %q", hits[0].Chunk)
	}

	// The grounding adapter surfaces the same content down the core.Knowledge seam.
	ground := kb.Grounding().Query("how long is the return window", 4)
	if len(ground) == 0 || !strings.Contains(ground[0].Content, "17 days") {
		t.Fatalf("grounding adapter did not surface the relevant doc: %+v", ground)
	}
}

func TestKnowledgeIngestIsIdempotentByID(t *testing.T) {
	kb := newKnowledgeBase(t)
	ctx := t.Context()
	mustIngest(t, kb, "doc-x", "original placeholder text", "x.md")
	mustIngest(t, kb, "doc-x", "the refreshed payload mentions wombats", "x.md")

	hits, err := kb.Query(ctx, "refreshed payload wombats", 4)
	if err != nil {
		t.Fatalf("Query: %v", err)
	}
	var docX int
	var sawWombats bool
	for _, h := range hits {
		if h.DocumentID == "doc-x" {
			docX++
			if strings.Contains(h.Chunk, "wombats") {
				sawWombats = true
			}
		}
	}
	if docX != 1 {
		t.Fatalf("expected exactly one doc-x row, got %d", docX)
	}
	if !sawWombats {
		t.Fatal("doc-x did not reflect the refreshed (upserted) content")
	}
}

func mustIngest(t *testing.T, kb *PostgresKnowledgeBase, id, content, source string) {
	t.Helper()
	if err := kb.Ingest(t.Context(), KnowledgeDocument{ID: id, Content: content, Source: source}); err != nil {
		t.Fatalf("Ingest(%s): %v", id, err)
	}
}

// ── ACL leak contract ────────────────────────────────────────────────────────

func withGroups(groups ...string) AccessContext {
	return AccessContext{
		Principal:   Principal{Sub: "u", Org: "acme", Role: "basic", Groups: groups},
		IsAnonymous: len(groups) == 0,
	}
}

func seededAclStore(t *testing.T) *PostgresAclKnowledgeStore {
	t.Helper()
	store := newAclKnowledgeStore(t)
	ctx := t.Context()
	if err := store.Ingest(ctx, KnowledgeDocument{ID: "pub", Content: "Public support hours are 9 to 5.", Source: "public.md"}, PublicAcl); err != nil {
		t.Fatalf("Ingest pub: %v", err)
	}
	if err := store.Ingest(ctx,
		KnowledgeDocument{ID: "secret", Content: "The private launch code is hunter2.", Source: "acme/private/launch.md"},
		AclForGroups("github:acme/private")); err != nil {
		t.Fatalf("Ingest secret: %v", err)
	}
	return store
}

func TestAclKnowledgeAnonymousSeesOnlyPublic(t *testing.T) {
	store := seededAclStore(t)
	hits, err := store.ForAccess(AnonymousAccess).Query(t.Context(), "private launch code", 10)
	if err != nil {
		t.Fatalf("Query: %v", err)
	}
	if containsDoc(hits, "secret") {
		t.Fatal("anonymous caller leaked the restricted 'secret' document")
	}
}

func TestAclKnowledgeEntitledUserReadsPrivateDoc(t *testing.T) {
	store := seededAclStore(t)
	hits, err := store.ForAccess(withGroups("github:acme/private")).Query(t.Context(), "private launch code", 10)
	if err != nil {
		t.Fatalf("Query: %v", err)
	}
	var ok bool
	for _, h := range hits {
		if h.DocumentID == "secret" && strings.Contains(h.Chunk, "hunter2") {
			ok = true
		}
	}
	if !ok {
		t.Fatalf("entitled caller could not read the private doc: %+v", hits)
	}
}

func TestAclKnowledgeUnentitledUserNoLeak(t *testing.T) {
	store := seededAclStore(t)
	hits, err := store.ForAccess(withGroups("github:acme/other")).Query(t.Context(), "private launch code hunter2", 10)
	if err != nil {
		t.Fatalf("Query: %v", err)
	}
	if containsDoc(hits, "secret") {
		t.Fatal("unentitled caller leaked the restricted 'secret' document")
	}
}

func containsDoc(hits []KnowledgeResult, id string) bool {
	for _, h := range hits {
		if h.DocumentID == id {
			return true
		}
	}
	return false
}
