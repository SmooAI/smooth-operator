---
"@smooai/smooth-operator": patch
---

fix(ingestion): the chunking contract (G2) — and the four ways the chunker was wrong

Gap **G2** was recorded as open ("our knowledge store assumes pre-chunked text"),
but the `Chunker` the connectors feed shipped with G1. What was actually missing
was the **contract suite** the gap doc asked for — and writing it found the
chunker wrong on four counts. Every one of them fails *silently*: no error, no
panic, just worse retrieval or a chunk the embedding API quietly truncates.

`rust/ingestion/tests/chunking.rs` (16 tests) pins chunk count / dense indices /
stable ids, overlap as a shared run of words, metadata + title + source + acl
propagation onto every chunk, oversized spill with no word lost, UTF-8 integrity
at the boundary, and degenerate configs. Fixed against it:

- **`max_chars` is a hard cap on the emitted chunk, overlap included.** Overlap
  was prepended on top of a chunk that already filled the cap, so a default
  500/64 chunker emitted chunks of up to 565 characters. The cap is the contract
  with the embedding model's input limit — over it, the API truncates rather
  than rejects, so the tail of a chunk is dropped from the index with nothing
  logged. Overlap now comes out of the packing budget.
- **Text without spaces now spills.** Splitting on word boundaries only meant a
  Chinese, Japanese or Thai document — or a long URL, or a minified blob — had
  exactly one "word", so it came back as a **single unbounded chunk**. A 50k-char
  document became one chunk, embedded truncated, retrievable as nothing useful.
  The fallback cuts on **character** boundaries.
- **CRLF documents split on paragraphs.** `"\r\n\r\n"` contains no `"\n\n"`, so
  every Windows-authored file and many HTTP-fetched pages arrived as one giant
  paragraph and lost all paragraph structure. Content is CRLF-normalized first.
- **A markdown heading is a hard chunk boundary.** A chunk spanning two sections
  attributes section A's text to section B's heading at retrieval time.

Characters, never bytes, throughout: slicing to a byte offset to hit a character
cap cuts an em-dash or an emoji in half — a panic in Rust, silent mojibake in a
port to the other four languages.

Proof of red: reverting `chunker.rs` in place, leaving the tests, fails 6 of 16.
The UTF-8 guard needed a second pass and is worth calling out — built from
space-separated words it passed against a deliberately byte-slicing
implementation, because spaced text never reaches the character-split path at
all. Rebuilt on unspaced mixed-width text it panics on that same mutant. A guard
that cannot fail is the defect, not the reassurance.

Rust only. Chunking runs server-side in the ingestion crate; the other four
languages have no ingestion pipeline to port it into, so there is nothing to
port yet. No public API change — `Chunker::new` / `chunk` keep their signatures.
