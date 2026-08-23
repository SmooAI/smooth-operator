//! Document → chunks (feature gap G2).
//!
//! The engine's `KnowledgeBase::ingest` chunks internally, but that's tuned for
//! whole pre-formed documents. The ingestion pipeline owns its own chunker so
//! the chunk shape (size cap, overlap, stable ids, propagated metadata) is
//! explicit and tested independently of any storage backend.
//!
//! ## Strategy
//!
//! 1. Normalize CRLF, then split content into paragraphs on blank lines.
//! 2. Greedily pack paragraphs into a chunk, **breaking before a markdown
//!    heading** so one chunk never straddles two sections.
//! 3. An oversized paragraph spills on word boundaries; a single "word" longer
//!    than the budget (a URL, or any script that does not use spaces — Chinese,
//!    Japanese, Thai) spills on *character* boundaries. Characters, never bytes:
//!    slicing an em-dash or an emoji in half is a panic in Rust and mojibake in
//!    a port.
//! 4. Successive chunks overlap by [`Chunker::overlap_chars`] of trailing text
//!    (carried as whole words) so a fact spanning a boundary stays retrievable.
//!
//! [`Chunker::max_chars`] is a **hard cap on the emitted chunk**, overlap
//! included: it is the contract with the embedding model's input limit, and a
//! chunk that exceeds it is silently truncated by the API rather than rejected.
//! Overlap is therefore spent out of the packing budget, not added on top of it.
//!
//! Each [`Chunk`] gets a **stable id** — `"{doc_id}#{index}"` — and inherits the
//! source document's title/metadata/acl, so retrieval can attribute and (later)
//! access-control every chunk.

use std::collections::HashMap;

use crate::connector::RawDocument;

/// Default maximum characters per chunk. Matches the engine's in-memory
/// `MAX_CHUNK_CHARS` so chunk granularity is consistent end to end.
pub const DEFAULT_MAX_CHARS: usize = 500;

/// Default overlap (characters of trailing text repeated into the next chunk).
pub const DEFAULT_OVERLAP_CHARS: usize = 64;

/// One chunk produced from a [`RawDocument`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// Stable id: `"{doc_id}#{index}"`.
    pub id: String,
    /// The originating document's id.
    pub document_id: String,
    /// 0-based position within the document.
    pub index: usize,
    /// The chunk text.
    pub text: String,
    /// Title/metadata/acl propagated from the source document. `title` (if any)
    /// is also stored under the `"title"` metadata key for retrieval display.
    pub metadata: HashMap<String, String>,
    /// Access-control labels propagated from the source document (G3).
    pub acl: Option<Vec<String>>,
}

/// Splits documents into overlapping, size-capped chunks.
#[derive(Debug, Clone)]
pub struct Chunker {
    max_chars: usize,
    overlap_chars: usize,
}

impl Chunker {
    /// Build with explicit `max_chars` and `overlap_chars`.
    ///
    /// `overlap_chars` is clamped below `max_chars` so a chunk always makes
    /// forward progress (an overlap ≥ size would loop forever).
    #[must_use]
    pub fn new(max_chars: usize, overlap_chars: usize) -> Self {
        let max_chars = max_chars.max(1);
        Self {
            max_chars,
            overlap_chars: overlap_chars.min(max_chars.saturating_sub(1)),
        }
    }

    /// The configured max characters per chunk.
    #[must_use]
    pub fn max_chars(&self) -> usize {
        self.max_chars
    }

    /// The configured overlap in characters.
    #[must_use]
    pub fn overlap_chars(&self) -> usize {
        self.overlap_chars
    }

    /// Characters available for *packed content*, leaving room for the overlap
    /// the next chunk will prepend (plus its joining space). This is what keeps
    /// `max_chars` a hard cap on the emitted chunk rather than on its content.
    fn pack_budget(&self) -> usize {
        if self.overlap_chars == 0 {
            self.max_chars
        } else {
            self.max_chars.saturating_sub(self.overlap_chars + 1).max(1)
        }
    }

    /// Chunk a [`RawDocument`], returning its ordered [`Chunk`]s.
    ///
    /// An empty / whitespace-only document yields no chunks.
    #[must_use]
    pub fn chunk(&self, doc: &RawDocument) -> Vec<Chunk> {
        let texts = self.split_text(&doc.content);

        // Build the per-chunk metadata once (title folded in), clone per chunk.
        let mut base_meta = doc.metadata.clone();
        if let Some(title) = &doc.title {
            base_meta
                .entry("title".to_string())
                .or_insert_with(|| title.clone());
        }
        base_meta
            .entry("source".to_string())
            .or_insert_with(|| doc.source.clone());

        texts
            .into_iter()
            .enumerate()
            .map(|(index, text)| Chunk {
                id: format!("{}#{index}", doc.id),
                document_id: doc.id.clone(),
                index,
                text,
                metadata: base_meta.clone(),
                acl: doc.acl.clone(),
            })
            .collect()
    }

    /// Split raw content into chunk-sized texts (no metadata; pure string work).
    fn split_text(&self, content: &str) -> Vec<String> {
        // CRLF-authored files and many HTTP responses separate paragraphs with
        // "\r\n\r\n", which contains no "\n\n" at all — without this, every
        // Windows-authored document arrives as one giant paragraph.
        let content = content.replace("\r\n", "\n");
        let budget = self.pack_budget();

        // 1. Paragraph units (blank-line separated), oversized ones spilled.
        let mut units: Vec<String> = Vec::new();
        for para in content.split("\n\n") {
            let trimmed = para.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.chars().count() <= budget {
                units.push(trimmed.to_string());
            } else {
                units.extend(self.spill(trimmed, budget));
            }
        }

        // 2. Greedily pack units, then 3. add trailing-word overlap.
        let mut chunks: Vec<String> = Vec::new();
        let mut current = String::new();
        for unit in units {
            if current.is_empty() {
                current = unit;
            } else if is_heading(&unit) {
                // A chunk spanning two sections attributes section A's text to
                // section B's heading at retrieval time. Break before headings.
                chunks.push(std::mem::take(&mut current));
                current = unit;
            } else if current.chars().count() + 2 + unit.chars().count() <= budget {
                current.push_str("\n\n");
                current.push_str(&unit);
            } else {
                chunks.push(std::mem::take(&mut current));
                current = unit;
            }
        }
        if !current.is_empty() {
            chunks.push(current);
        }

        self.apply_overlap(chunks)
    }

    /// Spill one oversized paragraph into budget-sized pieces, preferring word
    /// boundaries and falling back to character boundaries for a single token
    /// that is itself too long.
    fn spill(&self, para: &str, budget: usize) -> Vec<String> {
        let mut out = Vec::new();
        let mut current = String::new();
        for word in para.split_whitespace() {
            for piece in split_oversized_word(word, budget) {
                if current.is_empty() {
                    current = piece;
                } else if current.chars().count() + 1 + piece.chars().count() > budget {
                    out.push(std::mem::take(&mut current));
                    current = piece;
                } else {
                    current.push(' ');
                    current.push_str(&piece);
                }
            }
        }
        if !current.is_empty() {
            out.push(current);
        }
        out
    }

    /// Prepend the trailing `overlap_chars` (rounded to whole words) of each
    /// chunk onto the next, so a boundary-spanning fact appears in both — never
    /// pushing the result past `max_chars`.
    fn apply_overlap(&self, chunks: Vec<String>) -> Vec<String> {
        if self.overlap_chars == 0 || chunks.len() < 2 {
            return chunks;
        }
        let mut out = Vec::with_capacity(chunks.len());
        for (i, chunk) in chunks.iter().enumerate() {
            if i == 0 {
                out.push(chunk.clone());
                continue;
            }
            // Whatever room is left under the cap, never more than the overlap.
            let room = self
                .max_chars
                .saturating_sub(chunk.chars().count() + 1)
                .min(self.overlap_chars);
            let tail = trailing_words(&chunks[i - 1], room);
            if tail.is_empty() {
                out.push(chunk.clone());
            } else {
                out.push(format!("{tail} {chunk}"));
            }
        }
        out
    }
}

/// A markdown ATX heading line (`# `, `## `, …) — a hard chunk boundary.
fn is_heading(unit: &str) -> bool {
    unit.starts_with('#')
}

/// Split one word into pieces of at most `budget` **characters**.
///
/// A word that fits is returned whole. One that does not — a long URL, a
/// minified blob, or a run of Chinese/Japanese/Thai, none of which contain a
/// space to break on — is cut on character boundaries. `chars()` is what makes
/// that safe: slicing the same string by bytes would cut a multi-byte codepoint
/// in half.
fn split_oversized_word(word: &str, budget: usize) -> Vec<String> {
    if word.chars().count() <= budget {
        return vec![word.to_string()];
    }
    let chars: Vec<char> = word.chars().collect();
    chars
        .chunks(budget.max(1))
        .map(|piece| piece.iter().collect())
        .collect()
}

/// The last whole words of `s` totaling at most `limit` characters.
fn trailing_words(s: &str, limit: usize) -> String {
    if limit == 0 {
        return String::new();
    }
    let words: Vec<&str> = s.split_whitespace().collect();
    let mut take = 0usize;
    let mut len = 0usize;
    for word in words.iter().rev() {
        let add = word.chars().count() + usize::from(take > 0);
        if len + add > limit {
            break;
        }
        len += add;
        take += 1;
    }
    if take == 0 {
        return String::new();
    }
    words[words.len() - take..].join(" ")
}

impl Default for Chunker {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_CHARS, DEFAULT_OVERLAP_CHARS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiny_doc_is_a_single_chunk() {
        let doc = RawDocument::new("d", "test", "just a short note");
        let chunks = Chunker::default().chunk(&doc);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "just a short note");
        assert_eq!(chunks[0].id, "d#0");
        assert_eq!(chunks[0].index, 0);
        assert_eq!(chunks[0].document_id, "d");
    }

    #[test]
    fn empty_doc_yields_no_chunks() {
        let doc = RawDocument::new("d", "test", "   \n\n   ");
        assert!(Chunker::default().chunk(&doc).is_empty());
    }

    #[test]
    fn paragraphs_pack_then_split_at_cap() {
        // max 20 chars, no overlap → each ~15-char paragraph is its own chunk
        // because two won't fit (15 + 2 + 15 > 20).
        let chunker = Chunker::new(20, 0);
        let doc = RawDocument::new(
            "d",
            "test",
            "paragraph one!!\n\nparagraph two!!\n\nparagraph thr!!",
        );
        let chunks = chunker.chunk(&doc);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].text, "paragraph one!!");
        assert_eq!(chunks[1].text, "paragraph two!!");
        assert_eq!(chunks[2].index, 2);
    }

    #[test]
    fn small_paragraphs_pack_into_one_chunk() {
        let chunker = Chunker::new(100, 0);
        let doc = RawDocument::new("d", "test", "aaa\n\nbbb\n\nccc");
        let chunks = chunker.chunk(&doc);
        assert_eq!(chunks.len(), 1, "small paragraphs should pack together");
        assert!(chunks[0].text.contains("aaa"));
        assert!(chunks[0].text.contains("ccc"));
    }

    #[test]
    fn oversized_paragraph_hard_splits_on_words() {
        let chunker = Chunker::new(10, 0);
        // One paragraph, no blank lines, longer than the cap.
        let doc = RawDocument::new("d", "test", "alpha beta gamma delta epsilon");
        let chunks = chunker.chunk(&doc);
        assert!(chunks.len() > 1, "oversized paragraph must split");
        for c in &chunks {
            assert!(
                c.text.chars().count() <= 10,
                "chunk exceeds cap: {:?}",
                c.text
            );
        }
    }

    #[test]
    fn overlap_carries_trailing_words_into_next_chunk() {
        let chunker = Chunker::new(20, 8);
        let doc = RawDocument::new(
            "d",
            "test",
            "first chunk text\n\nsecond chunk text\n\nthird chunk text",
        );
        let chunks = chunker.chunk(&doc);
        assert!(chunks.len() >= 2);
        // The second chunk should begin with a trailing word of the first.
        let prev_last = chunks[0]
            .text
            .split_whitespace()
            .last()
            .unwrap()
            .to_string();
        assert!(
            chunks[1].text.starts_with(&prev_last),
            "expected overlap word {prev_last:?} at start of {:?}",
            chunks[1].text
        );
    }

    #[test]
    fn overlap_is_clamped_below_max_so_it_terminates() {
        // overlap >= max would loop; constructor clamps it.
        let chunker = Chunker::new(10, 999);
        assert!(chunker.overlap_chars() < chunker.max_chars());
        let doc = RawDocument::new("d", "test", "alpha beta gamma delta epsilon zeta");
        let chunks = chunker.chunk(&doc); // must terminate
        assert!(!chunks.is_empty());
    }

    #[test]
    fn metadata_and_title_propagate_to_every_chunk() {
        let chunker = Chunker::new(15, 0);
        let doc = RawDocument::new("d", "wiki", "alpha words here\n\nbeta words here")
            .with_title("My Title")
            .with_metadata("category", "facts")
            .with_acl(vec!["group-a".to_string()]);
        let chunks = chunker.chunk(&doc);
        assert!(chunks.len() >= 2);
        for c in &chunks {
            assert_eq!(
                c.metadata.get("title").map(String::as_str),
                Some("My Title")
            );
            assert_eq!(
                c.metadata.get("category").map(String::as_str),
                Some("facts")
            );
            assert_eq!(c.metadata.get("source").map(String::as_str), Some("wiki"));
            assert_eq!(c.acl.as_deref(), Some(&["group-a".to_string()][..]));
        }
    }

    #[test]
    fn chunk_ids_are_stable_and_indexed() {
        let chunker = Chunker::new(15, 0);
        let doc = RawDocument::new("doc-42", "test", "alpha words!!\n\nbeta words!!");
        let chunks = chunker.chunk(&doc);
        assert_eq!(chunks[0].id, "doc-42#0");
        assert_eq!(chunks[1].id, "doc-42#1");
        // Re-chunking the same input yields the same ids (stable).
        let again = chunker.chunk(&doc);
        assert_eq!(chunks, again);
    }
}
