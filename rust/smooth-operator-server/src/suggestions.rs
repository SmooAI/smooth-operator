//! Suggested quick replies — the model→`suggestedNextActions` plumbing.
//!
//! The protocol's `eventual_response` has always carried a
//! `suggestedNextActions` array on its `GeneralAgentResponse`-shaped payload,
//! but the reference runtime hardcoded it empty. This module makes it live,
//! prompt-driven:
//!
//! 1. [`SUGGESTED_REPLIES_PROMPT_SECTION`] is appended to every turn's system
//!    prompt, instructing the model to end its reply with one machine-parsed
//!    trailer line: `<suggested_replies>["…","…"]</suggested_replies>`.
//! 2. [`MarkerSuppressor`] holds back streamed tokens that could be (part of)
//!    that trailer so the raw marker never flashes in a client's live stream.
//! 3. [`extract_suggested_replies`] strips the trailer from the final reply and
//!    returns the parsed suggestions, which the runner threads onto
//!    [`TurnResult`](crate::runner::TurnResult) and into the
//!    `eventual_response`'s `suggestedNextActions`.
//!
//! Everything degrades to exactly the old behavior when the model emits no
//! trailer: nothing suppressed, nothing stripped, empty suggestions.

/// Opening tag of the machine-parsed trailer.
pub const MARKER_OPEN: &str = "<suggested_replies>";
/// Closing tag of the machine-parsed trailer.
pub const MARKER_CLOSE: &str = "</suggested_replies>";
/// Maximum suggestions surfaced per turn (clients typically render ≤ 4 chips).
pub const MAX_SUGGESTIONS: usize = 4;

/// System-prompt section teaching the model the trailer contract. Appended
/// unconditionally by the runner; a model that emits no trailer loses nothing.
pub const SUGGESTED_REPLIES_PROMPT_SECTION: &str = "\
## Suggested quick replies

When your reply asks the user a question (or a small set of likely responses exists), \
end your reply with ONE final line of this exact machine-parsed form:

<suggested_replies>[\"First candidate reply\", \"Second candidate reply\"]</suggested_replies>

Rules:
- 2 to 4 candidate replies, each a short first-person answer the USER might tap (under 60 characters each), written from the user's point of view.
- Offer meaningfully different options (e.g. points on a scale, yes/no with nuance), not rephrasings of one answer.
- The line is stripped before display and never shown to the user. Never mention it, never explain it, never place it anywhere but the very end.
- Omit the line entirely when no quick reply makes sense (e.g. you asked for a name or a free-form description).";

/// Strip the `<suggested_replies>…</suggested_replies>` trailer from a final
/// reply, returning the clean reply and the parsed suggestions.
///
/// Tolerant by design:
/// - no opening tag → reply unchanged, no suggestions;
/// - unparseable / non-array JSON between the tags → the span is still
///   stripped (never show the user raw machinery) but yields no suggestions;
/// - missing closing tag (truncated stream) → strip from the opening tag to
///   the end of the reply;
/// - entries are trimmed, empties dropped, capped at [`MAX_SUGGESTIONS`].
#[must_use]
pub fn extract_suggested_replies(reply: &str) -> (String, Vec<String>) {
    let Some(start) = reply.rfind(MARKER_OPEN) else {
        // Some models (gpt-oss-120b) ignore the marker contract and instead end
        // the reply with a markdown "Suggested replies:" list. Parse that as a
        // fallback so chips still populate; otherwise leave the reply untouched.
        // ponytail: responseParts is cleaned here; the raw list can still flash in
        // the live token stream (MarkerSuppressor only hides <suggested_replies>) —
        // pearl th-gptoss-chips-stream if that becomes visible enough to matter.
        return extract_markdown_suggested_replies(reply)
            .unwrap_or_else(|| (reply.to_string(), Vec::new()));
    };
    let after_open = start + MARKER_OPEN.len();
    let (body, end) = match reply[after_open..].find(MARKER_CLOSE) {
        Some(rel) => (
            &reply[after_open..after_open + rel],
            after_open + rel + MARKER_CLOSE.len(),
        ),
        None => (&reply[after_open..], reply.len()),
    };
    let mut clean = String::with_capacity(reply.len());
    clean.push_str(&reply[..start]);
    clean.push_str(&reply[end..]);
    let clean = clean.trim_end().to_string();

    let suggestions = serde_json::from_str::<Vec<serde_json::Value>>(body.trim())
        .map(|items| {
            items
                .into_iter()
                .filter_map(|v| match v {
                    serde_json::Value::String(s) => {
                        let t = s.trim().to_string();
                        (!t.is_empty()).then_some(t)
                    }
                    _ => None,
                })
                .take(MAX_SUGGESTIONS)
                .collect()
        })
        .unwrap_or_default();
    (clean, suggestions)
}

/// Fallback for models that ignore the `<suggested_replies>` marker and instead
/// end the reply with a markdown `Suggested replies:` list (observed on
/// gpt-oss-120b, TP agent's default model). Returns the cleaned reply (list
/// stripped) + parsed items, or `None` when there is no such trailing list.
/// Conservative: fires only when a `suggested replies:` header is followed by a
/// pure bullet/numbered list to the end of the reply, so a normal list in the
/// body is never mistaken for chips.
fn extract_markdown_suggested_replies(reply: &str) -> Option<(String, Vec<String>)> {
    let (start, header_len) = ["suggested replies:", "suggested_replies:"]
        .iter()
        .filter_map(|h| rfind_ci(reply, h).map(|p| (p, h.len())))
        .max_by_key(|&(p, _)| p)?;
    // The header must own its line — the list starts on the NEXT line. Anything
    // else after the header on the same line means it's prose, not a trailer.
    let after = &reply[start + header_len..];
    let nl = after.find('\n')?;
    let mut items = Vec::new();
    for line in after[nl + 1..].lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        // Every non-empty line must be a bullet (-, *, •) or "N." / "N)" item;
        // any prose line means this isn't a clean chip trailer.
        let bulleted = t.trim_start_matches(['-', '*', '•']);
        let item = if bulleted.len() < t.len() {
            bulleted.trim_start()
        } else if let Some(i) = t
            .find(['.', ')'])
            .filter(|&i| i > 0 && t[..i].bytes().all(|b| b.is_ascii_digit()))
        {
            t[i + 1..].trim_start()
        } else {
            return None;
        };
        let item = item.trim().trim_matches(['"', '\'', '`']).trim();
        if item.is_empty() {
            return None;
        }
        items.push(item.to_string());
    }
    if items.is_empty() {
        return None;
    }
    let clean = reply[..start].trim_end().to_string();
    Some((clean, items.into_iter().take(MAX_SUGGESTIONS).collect()))
}

/// Case-insensitive `rfind` returning a byte offset into `haystack` (ASCII
/// needle ⇒ the offset is always a char boundary). Avoids allocating a
/// lowercased copy, which would desync offsets on non-ASCII text.
fn rfind_ci(haystack: &str, needle_lower: &str) -> Option<usize> {
    let (hb, nb) = (haystack.as_bytes(), needle_lower.as_bytes());
    if nb.is_empty() || hb.len() < nb.len() {
        return None;
    }
    (0..=hb.len() - nb.len()).rev().find(|&i| {
        hb[i..i + nb.len()]
            .iter()
            .zip(nb)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
    })
}

/// Streaming hold-back so the trailer never flashes in the live token stream.
///
/// Feed each token delta through [`push`](Self::push) and forward only what it
/// returns. Text that could be the start of [`MARKER_OPEN`] is held until it
/// either mismatches (then flushed) or completes the marker (then everything
/// from the marker on is suppressed — the trailer is the reply's final line).
/// Call [`finish`](Self::finish) at stream end to flush a dangling partial
/// prefix that never became the marker.
#[derive(Debug, Default)]
pub struct MarkerSuppressor {
    held: String,
    suppressing: bool,
}

impl MarkerSuppressor {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a token delta; returns the text now safe to emit.
    pub fn push(&mut self, delta: &str) -> String {
        if self.suppressing {
            return String::new();
        }
        self.held.push_str(delta);
        if let Some(pos) = self.held.find(MARKER_OPEN) {
            self.suppressing = true;
            let emit = self.held[..pos].to_string();
            self.held.clear();
            return emit;
        }
        // Hold back the longest suffix that is a (proper) prefix of the marker.
        let held_from = longest_marker_prefix_suffix(&self.held);
        let emit = self.held[..held_from].to_string();
        self.held.drain(..held_from);
        emit
    }

    /// Stream ended: flush any held partial that never became the marker.
    #[must_use]
    pub fn finish(self) -> String {
        if self.suppressing {
            String::new()
        } else {
            self.held
        }
    }
}

/// Byte index where the longest suffix of `s` that is a prefix of
/// [`MARKER_OPEN`] begins (or `s.len()` when there is none). The marker is
/// ASCII, so suffix starts are checked at char boundaries only.
fn longest_marker_prefix_suffix(s: &str) -> usize {
    let max_len = MARKER_OPEN.len().min(s.len());
    for take in (1..=max_len).rev() {
        let start = s.len() - take;
        if s.is_char_boundary(start) && MARKER_OPEN.starts_with(&s[start..]) {
            return start;
        }
    }
    s.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_and_strips_trailer() {
        let (clean, sug) = extract_suggested_replies(
            "How mature are your processes?\n\n<suggested_replies>[\"Ad-hoc\", \"Repeatable\", \"Optimized\"]</suggested_replies>",
        );
        assert_eq!(clean, "How mature are your processes?");
        assert_eq!(sug, vec!["Ad-hoc", "Repeatable", "Optimized"]);
    }

    #[test]
    fn no_trailer_is_a_no_op() {
        let (clean, sug) = extract_suggested_replies("Just a plain reply.");
        assert_eq!(clean, "Just a plain reply.");
        assert!(sug.is_empty());
    }

    #[test]
    fn caps_at_max_and_drops_junk_entries() {
        let (_, sug) = extract_suggested_replies(
            "Q?\n<suggested_replies>[\"a\", 2, \"  \", \"b\", \"c\", \"d\", \"e\"]</suggested_replies>",
        );
        assert_eq!(sug, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn malformed_json_still_strips_the_span() {
        let (clean, sug) =
            extract_suggested_replies("Q?\n<suggested_replies>[oops</suggested_replies>");
        assert_eq!(clean, "Q?");
        assert!(sug.is_empty());
    }

    #[test]
    fn missing_close_tag_strips_to_end() {
        let (clean, sug) = extract_suggested_replies("Q?\n<suggested_replies>[\"a\", \"b\"");
        assert_eq!(clean, "Q?");
        assert!(sug.is_empty(), "truncated JSON parses to nothing");
    }

    #[test]
    fn gptoss_markdown_list_becomes_chips() {
        // Real gpt-oss-120b shape: ignores the marker, ends with a quoted list.
        let (clean, sug) = extract_suggested_replies(
            "Could you tell me your current process maturity level?\n\nsuggested_replies:\n- \"I'm at the initial stage.\"\n- \"We have defined processes.\"\n- \"Our processes are mature.\"\n- \"Explain the maturity model?\"",
        );
        assert_eq!(
            clean,
            "Could you tell me your current process maturity level?"
        );
        assert_eq!(
            sug,
            vec![
                "I'm at the initial stage.",
                "We have defined processes.",
                "Our processes are mature.",
                "Explain the maturity model?"
            ]
        );
    }

    #[test]
    fn markdown_list_handles_numbered_and_header_casing() {
        let (clean, sug) =
            extract_suggested_replies("Pick one.\n\n**Suggested Replies:**\n1. Yes\n2) No");
        assert_eq!(clean, "Pick one.\n\n**"); // trailing "**" before the header is left as-is
        assert_eq!(sug, vec!["Yes", "No"]);
    }

    #[test]
    fn markdown_fallback_ignores_a_body_list_without_the_header() {
        let reply = "Here are the steps:\n- do a\n- do b";
        let (clean, sug) = extract_suggested_replies(reply);
        assert_eq!(clean, reply); // no "suggested replies:" header ⇒ untouched
        assert!(sug.is_empty());
    }

    #[test]
    fn markdown_fallback_bails_when_header_is_followed_by_prose() {
        // A header followed by a non-list line is not a clean chip trailer.
        let reply = "Suggested replies: I don't have any right now.";
        let (clean, sug) = extract_suggested_replies(reply);
        assert_eq!(clean, reply);
        assert!(sug.is_empty());
    }

    #[test]
    fn suppressor_passes_plain_text_through() {
        let mut s = MarkerSuppressor::new();
        let mut out = String::new();
        for d in ["Hello ", "there, ", "how are you?"] {
            out.push_str(&s.push(d));
        }
        out.push_str(&s.finish());
        assert_eq!(out, "Hello there, how are you?");
    }

    #[test]
    fn suppressor_hides_marker_split_across_deltas() {
        let mut s = MarkerSuppressor::new();
        let mut out = String::new();
        for d in [
            "Pick one!\n",
            "<sugg",
            "ested_repl",
            "ies>[\"a\"]</sug",
            "gested_replies>",
        ] {
            out.push_str(&s.push(d));
        }
        out.push_str(&s.finish());
        assert_eq!(out, "Pick one!\n");
    }

    #[test]
    fn suppressor_flushes_false_prefix() {
        let mut s = MarkerSuppressor::new();
        let mut out = String::new();
        // "<sup" shares "<su" with the marker but then mismatches.
        for d in ["a ", "<sup", "erb> tag"] {
            out.push_str(&s.push(d));
        }
        out.push_str(&s.finish());
        assert_eq!(out, "a <superb> tag");
    }

    #[test]
    fn suppressor_flushes_dangling_partial_on_finish() {
        let mut s = MarkerSuppressor::new();
        let mut out = String::new();
        out.push_str(&s.push("reply ends with <suggested_rep"));
        out.push_str(&s.finish());
        assert_eq!(out, "reply ends with <suggested_rep");
    }

    #[test]
    fn suppressor_handles_multibyte_text() {
        let mut s = MarkerSuppressor::new();
        let mut out = String::new();
        for d in ["héllo ✨", "<suggested_replies>[\"a\"]</suggested_replies>"] {
            out.push_str(&s.push(d));
        }
        out.push_str(&s.finish());
        assert_eq!(out, "héllo ✨");
    }
}
