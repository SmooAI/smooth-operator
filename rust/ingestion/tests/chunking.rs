//! Chunking-pipeline contract (feature gap G2).
//!
//! The unit tests inside `src/chunker.rs` cover the happy path on ASCII prose.
//! This suite is the *contract*: the invariants a downstream embedder and a
//! retrieval path depend on, exercised over the inputs real connectors actually
//! deliver — CRLF-authored files, scripts without spaces, and text whose chunk
//! boundary lands on a multi-byte codepoint.
//!
//! Every assertion here is one the pipeline would silently violate rather than
//! fail loudly: an over-cap chunk gets truncated by the embedding API, a
//! never-split CJK document becomes one useless 100k-char "chunk", and a
//! codepoint sliced in half is a panic or mojibake far from the cause.

use smooth_operator_ingestion::{Chunk, Chunker, RawDocument};

/// Characters (not bytes) — the unit the chunker's cap is denominated in.
fn chars(s: &str) -> usize {
    s.chars().count()
}

fn text_of(chunks: &[Chunk]) -> Vec<&str> {
    chunks.iter().map(|c| c.text.as_str()).collect()
}

// ---------------------------------------------------------------------------
// Chunk count, order, identity
// ---------------------------------------------------------------------------

#[test]
fn chunk_count_matches_the_content_and_indices_are_dense() {
    // Five paragraphs of ~30 chars with a 40-char cap and no overlap: no two
    // paragraphs fit together, so the count is exactly the paragraph count.
    let content = (0..5)
        .map(|i| format!("paragraph number {i} of the doc"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let doc = RawDocument::new("doc-a", "test", content);
    let chunks = Chunker::new(40, 0).chunk(&doc);

    assert_eq!(chunks.len(), 5, "got {:?}", text_of(&chunks));
    for (i, c) in chunks.iter().enumerate() {
        assert_eq!(c.index, i, "indices must be dense and ordered");
        assert_eq!(c.id, format!("doc-a#{i}"), "id is doc-scoped + positional");
        assert_eq!(c.document_id, "doc-a");
    }
}

#[test]
fn chunking_is_deterministic() {
    let doc = RawDocument::new("doc-d", "test", "alpha beta\n\ngamma delta\n\nepsilon zeta");
    let chunker = Chunker::new(20, 5);
    assert_eq!(chunker.chunk(&doc), chunker.chunk(&doc));
}

// ---------------------------------------------------------------------------
// The size cap — the invariant the embedder depends on
// ---------------------------------------------------------------------------

#[test]
fn no_chunk_exceeds_the_cap_even_with_overlap_on() {
    // The cap is the contract with the embedding model's token limit. Overlap
    // is a retrieval nicety; it must be spent *inside* the budget, never added
    // on top of a chunk that already fills it.
    let content = (0..12)
        .map(|i| format!("paragraph {i} carries enough words to nearly fill a chunk by itself"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let doc = RawDocument::new("doc-cap", "test", content);

    for (max, overlap) in [(80usize, 20usize), (120, 40), (500, 64), (40, 15)] {
        let chunks = Chunker::new(max, overlap).chunk(&doc);
        assert!(!chunks.is_empty());
        for c in &chunks {
            assert!(
                chars(&c.text) <= max,
                "cap {max}/overlap {overlap}: chunk is {} chars: {:?}",
                chars(&c.text),
                c.text
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Overlap
// ---------------------------------------------------------------------------

#[test]
fn successive_chunks_share_trailing_context() {
    let content = (0..6)
        .map(|i| format!("sentence {i} about widgets and gears"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let doc = RawDocument::new("doc-o", "test", content);
    let chunks = Chunker::new(60, 20).chunk(&doc);

    assert!(chunks.len() >= 3, "need several chunks to test overlap");
    for pair in chunks.windows(2) {
        // The invariant is a shared *run of words*, not one word: chunk N+1
        // must open with some non-empty word sequence that chunk N closes with.
        let next_words: Vec<&str> = pair[1].text.split_whitespace().collect();
        let shared = (1..=next_words.len())
            .rev()
            .map(|n| next_words[..n].join(" "))
            .find(|prefix| pair[0].text.ends_with(prefix.as_str()));
        let shared = shared.unwrap_or_else(|| {
            panic!(
                "chunk {} shares no leading text with the previous chunk\n prev: {:?}\n next: {:?}",
                pair[1].index, pair[0].text, pair[1].text
            )
        });
        assert!(
            shared.chars().count() <= 20,
            "overlap {shared:?} exceeds the configured 20 chars"
        );
    }
}

#[test]
fn zero_overlap_means_no_repetition() {
    let doc = RawDocument::new("doc-z", "test", "alpha alpha\n\nbeta beta\n\ngamma gamma");
    let chunks = Chunker::new(15, 0).chunk(&doc);
    assert_eq!(
        text_of(&chunks),
        vec!["alpha alpha", "beta beta", "gamma gamma"]
    );
}

// ---------------------------------------------------------------------------
// Boundary rules
// ---------------------------------------------------------------------------

#[test]
fn a_markdown_heading_starts_a_new_chunk() {
    // A chunk that straddles two sections attributes section A's text to
    // section B's heading at retrieval time. The heading is a hard boundary.
    let content = "## Refunds\n\nRefunds take five days.\n\n## Shipping\n\nShipping is free.";
    let doc = RawDocument::new("doc-h", "test", content);
    // Cap is wide enough that a naive packer would merge all four paragraphs.
    let chunks = Chunker::new(500, 0).chunk(&doc);

    assert!(
        chunks.len() >= 2,
        "the two sections must not share a chunk: {:?}",
        text_of(&chunks)
    );
    let refunds = chunks.iter().find(|c| c.text.contains("Refunds")).unwrap();
    assert!(
        !refunds.text.contains("Shipping"),
        "section bleed: {:?}",
        refunds.text
    );
}

#[test]
fn crlf_documents_split_on_paragraphs_like_lf_ones() {
    // Windows-authored files and many HTTP responses use CRLF. If the splitter
    // only knows "\n\n", the whole document is one paragraph and paragraph
    // structure is lost for every such source.
    let lf = "alpha paragraph\n\nbeta paragraph\n\ngamma paragraph";
    let crlf = "alpha paragraph\r\n\r\nbeta paragraph\r\n\r\ngamma paragraph";
    let chunker = Chunker::new(20, 0);

    let lf_chunks = chunker.chunk(&RawDocument::new("d", "test", lf));
    let crlf_chunks = chunker.chunk(&RawDocument::new("d", "test", crlf));

    assert_eq!(
        text_of(&crlf_chunks),
        text_of(&lf_chunks),
        "CRLF must chunk identically to LF"
    );
    for c in &crlf_chunks {
        assert!(
            !c.text.contains('\r'),
            "stray CR in chunk text: {:?}",
            c.text
        );
    }
}

// ---------------------------------------------------------------------------
// Oversized items spill
// ---------------------------------------------------------------------------

#[test]
fn an_oversized_paragraph_spills_across_chunks_without_losing_words() {
    let words: Vec<String> = (0..200).map(|i| format!("w{i}")).collect();
    let doc = RawDocument::new("doc-s", "test", words.join(" "));
    let chunks = Chunker::new(50, 0).chunk(&doc);

    assert!(chunks.len() > 1, "oversized paragraph must spill");
    let rejoined: Vec<String> = chunks
        .iter()
        .flat_map(|c| c.text.split_whitespace().map(str::to_string))
        .collect();
    assert_eq!(
        rejoined, words,
        "spilling must preserve every word, in order"
    );
}

#[test]
fn text_with_no_whitespace_still_spills() {
    // Chinese, Japanese and Thai carry no spaces, and so do minified payloads
    // and long URLs. A word-boundary-only splitter returns the whole document
    // as one chunk — silently, with no error — and the embedder truncates it.
    let doc = RawDocument::new(
        "doc-cjk",
        "test",
        "宽带上网服务的开通与故障处理流程说明".repeat(20),
    );
    let chunks = Chunker::new(60, 0).chunk(&doc);

    assert!(
        chunks.len() > 1,
        "unspaced text must still spill, got {} chunk(s)",
        chunks.len()
    );
    for c in &chunks {
        assert!(chars(&c.text) <= 60, "chunk is {} chars", chars(&c.text));
    }
}

#[test]
fn a_single_word_longer_than_the_cap_is_split_not_emitted_whole() {
    let long_token = "a".repeat(250);
    let doc = RawDocument::new("doc-t", "test", format!("prefix {long_token} suffix"));
    let chunks = Chunker::new(40, 0).chunk(&doc);
    for c in &chunks {
        assert!(chars(&c.text) <= 40, "chunk is {} chars", chars(&c.text));
    }
}

// ---------------------------------------------------------------------------
// UTF-8 integrity at the boundary
// ---------------------------------------------------------------------------

#[test]
fn chunk_boundaries_never_split_a_codepoint() {
    // The hazard: a splitter that slices by *bytes* to hit a character cap cuts
    // an em-dash or an emoji in half. In Rust that is an outright panic; in a
    // port of this logic it is silent mojibake.
    //
    // Reaching the byte-slicing path takes unspaced text — spaced text never
    // needs a character-level cut, so a test built from words would pass
    // against a byte-slicing implementation and guard nothing. The unit below
    // is deliberately mixed-width (1/2/3/4 bytes) so no cap can accidentally
    // land on a character boundary every time.
    let unit = "aé数🚀—";
    let content = unit.repeat(40);
    let doc = RawDocument::new("doc-u", "test", &content);

    for max in [7usize, 11, 17, 23] {
        let chunks = Chunker::new(max, 0).chunk(&doc);
        assert!(chunks.len() > 1, "cap {max}: expected the run to spill");
        let rejoined: String = chunks.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(
            rejoined, content,
            "cap {max}: spilling an unspaced run must reproduce it exactly"
        );
        assert!(
            !rejoined.contains('\u{FFFD}'),
            "cap {max}: replacement char — a codepoint was cut"
        );
        for ch in ['é', '数', '🚀', '—'] {
            assert_eq!(
                rejoined.matches(ch).count(),
                content.matches(ch).count(),
                "cap {max}: lost or duplicated {ch:?} at a chunk boundary"
            );
        }
        for c in &chunks {
            assert!(
                chars(&c.text) <= max,
                "cap {max}: chunk is {} chars",
                chars(&c.text)
            );
        }
    }
}

#[test]
fn spilling_multibyte_text_with_overlap_keeps_every_chunk_under_the_cap() {
    // Same hazard on the overlap path: the tail is measured in characters, so a
    // byte-denominated implementation would over- or under-count it.
    let doc = RawDocument::new("doc-uo", "test", "aé数🚀— ".repeat(40));
    let chunks = Chunker::new(24, 8).chunk(&doc);
    assert!(chunks.len() > 1);
    for c in &chunks {
        assert!(
            chars(&c.text) <= 24,
            "chunk is {} chars: {:?}",
            chars(&c.text),
            c.text
        );
    }
}

// ---------------------------------------------------------------------------
// Metadata propagation
// ---------------------------------------------------------------------------

#[test]
fn every_chunk_carries_the_documents_title_metadata_source_and_acl() {
    let doc = RawDocument::new(
        "doc-m",
        "confluence",
        "alpha paragraph here\n\nbeta paragraph here\n\ngamma paragraph here",
    )
    .with_title("Refund Policy")
    .with_metadata("space", "SUPPORT")
    .with_acl(vec!["group:support".to_string()]);

    let chunks = Chunker::new(25, 0).chunk(&doc);
    assert!(chunks.len() >= 3, "need several chunks");
    for c in &chunks {
        assert_eq!(
            c.metadata.get("title").map(String::as_str),
            Some("Refund Policy")
        );
        assert_eq!(c.metadata.get("space").map(String::as_str), Some("SUPPORT"));
        assert_eq!(
            c.metadata.get("source").map(String::as_str),
            Some("confluence")
        );
        assert_eq!(c.acl.as_deref(), Some(&["group:support".to_string()][..]));
    }
}

#[test]
fn explicit_metadata_wins_over_the_derived_title_and_source_keys() {
    let doc = RawDocument::new("doc-w", "web", "some content")
        .with_title("Derived")
        .with_metadata("title", "Explicit")
        .with_metadata("source", "explicit-source");
    let chunks = Chunker::default().chunk(&doc);
    assert_eq!(
        chunks[0].metadata.get("title").map(String::as_str),
        Some("Explicit")
    );
    assert_eq!(
        chunks[0].metadata.get("source").map(String::as_str),
        Some("explicit-source")
    );
}

// ---------------------------------------------------------------------------
// Degenerate input
// ---------------------------------------------------------------------------

#[test]
fn whitespace_only_and_empty_documents_yield_nothing() {
    for content in ["", "   ", "\n\n\n", "\r\n\r\n", " \t \n "] {
        let doc = RawDocument::new("d", "test", content);
        assert!(
            Chunker::default().chunk(&doc).is_empty(),
            "expected no chunks for {content:?}"
        );
    }
}

#[test]
fn a_degenerate_config_still_terminates_and_respects_its_cap() {
    // overlap >= max would loop forever if it were not clamped.
    let doc = RawDocument::new("d", "test", "alpha beta gamma delta epsilon zeta eta");
    for (max, overlap) in [(1usize, 999usize), (2, 999), (5, 5), (10, 9)] {
        let chunker = Chunker::new(max, overlap);
        let chunks = chunker.chunk(&doc);
        assert!(!chunks.is_empty(), "cap {max}/overlap {overlap}: no chunks");
        for c in &chunks {
            assert!(
                chars(&c.text) <= max.max(1),
                "cap {max}/overlap {overlap}: chunk is {} chars: {:?}",
                chars(&c.text),
                c.text
            );
        }
    }
}
