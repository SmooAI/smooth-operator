//! **Skill resolution seam** — the engine side of `send_message.skill`.
//!
//! A *skill* is a named, reusable recipe (a markdown body). Before this seam,
//! every client resolved the skill itself and prepended the body to the message
//! text, so the wire carried prose. Now the wire carries **intent** — `skill:
//! "code-review"` — and the server resolves it and composes it into the turn's
//! system prompt (see [`crate::runner::TurnRequest::skill_section`]), leaving the
//! persisted user message exactly what the user typed.
//!
//! Two pieces:
//!
//! - [`SkillResolver`] — the seam. A host (Big Smooth, a cloud flavor) installs
//!   its own via [`AppState::with_skill_resolver`](crate::state::AppState::with_skill_resolver)
//!   to resolve from wherever its skills live.
//! - [`DirSkillResolver`] — the working default: `<root>/<name>/SKILL.md` over
//!   the roots in `SMOOTH_SKILLS_DIR` (a `:`-separated path list, first match
//!   wins). Unset ⇒ no resolver is installed and any `skill` field is a clean
//!   `SKILL_NOT_FOUND` error, so a multi-tenant deploy never reads host skills
//!   by accident.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

/// Env var naming the skill roots for the default [`DirSkillResolver`]:
/// `:`-separated directories, searched in order.
pub const SKILLS_DIR_ENV: &str = "SMOOTH_SKILLS_DIR";

/// Resolves a skill name to its markdown body.
///
/// `None` means "unknown skill" — the handler turns that into a
/// `SKILL_NOT_FOUND` error and does **not** run the turn, so a typo'd skill
/// never silently degrades into an unskilled answer.
#[async_trait]
pub trait SkillResolver: Send + Sync {
    /// The skill's markdown body, or `None` when no such skill exists.
    async fn resolve(&self, name: &str) -> Option<String>;
}

/// Render a resolved skill as a system-prompt section.
///
/// The skill moved from the *user message* (where clients used to prepend it) to
/// the *system prompt*, so the framing line is what tells the model the skill
/// applies to this turn.
#[must_use]
pub fn skill_section(name: &str, body: &str) -> String {
    format!("## Skill: {name}\n\nThe user invoked this skill for this turn. Follow it.\n\n{body}")
}

/// Whether `name` is a legal skill name.
///
/// Deliberately strict: ASCII alphanumerics, `-` and `_` only. That is the
/// kebab-case convention skills already use, and it makes path traversal
/// (`..`, `/`, `\`, NUL) unrepresentable rather than filtered — the name is
/// joined onto a filesystem root by [`DirSkillResolver`].
#[must_use]
pub fn is_valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Strip a leading YAML frontmatter block (`---` … `---`), returning the body.
///
/// SKILL.md files carry frontmatter (description, triggers, allowed tools) that
/// is discovery metadata, not instructions — the model should see only the body.
fn strip_frontmatter(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("---\n") else {
        return text;
    };
    // The closing fence is a line that is exactly `---`.
    for (idx, line) in rest.match_indices("---") {
        let at_line_start = idx == 0 || rest.as_bytes()[idx - 1] == b'\n';
        let rest_of_line = &rest[idx + line.len()..];
        if at_line_start && rest_of_line.starts_with('\n') {
            return rest_of_line.trim_start_matches('\n');
        }
    }
    // Unterminated frontmatter — treat the whole thing as body rather than
    // swallowing the file.
    text
}

/// The default resolver: reads `<root>/<name>/SKILL.md`, first root wins.
pub struct DirSkillResolver {
    roots: Vec<PathBuf>,
}

impl DirSkillResolver {
    /// Build over an explicit list of roots, searched in order.
    #[must_use]
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }

    /// Build from [`SKILLS_DIR_ENV`] (a `:`-separated path list).
    ///
    /// Returns `None` when the var is unset or names no non-empty root, so the
    /// caller installs nothing and the feature stays off by default.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        Self::from_path_list(&std::env::var(SKILLS_DIR_ENV).ok()?)
    }

    /// Build from a `:`-separated path list (the parsed half of
    /// [`from_env`](Self::from_env), so it is testable without touching the
    /// process environment).
    #[must_use]
    pub fn from_path_list(list: &str) -> Option<Self> {
        let roots: Vec<PathBuf> = list
            .split(':')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();
        (!roots.is_empty()).then(|| Self::new(roots))
    }
}

#[async_trait]
impl SkillResolver for DirSkillResolver {
    async fn resolve(&self, name: &str) -> Option<String> {
        if !is_valid_skill_name(name) {
            return None;
        }
        for root in &self.roots {
            // ponytail: blocking read on the async runtime. A SKILL.md is a few
            // KB off local disk; move to `tokio::fs` if a resolver ever fronts
            // network storage.
            if let Ok(text) = std::fs::read_to_string(root.join(name).join("SKILL.md")) {
                let body = strip_frontmatter(&text).trim();
                if !body.is_empty() {
                    return Some(body.to_string());
                }
            }
        }
        None
    }
}

/// Resolve `name` through `resolver` and render it as a system-prompt section.
///
/// `None` when there is no resolver installed or the skill is unknown — both are
/// `SKILL_NOT_FOUND` to the client (the distinction is a deployment detail the
/// caller should not have to guess at).
pub async fn resolve_section(
    resolver: Option<&Arc<dyn SkillResolver>>,
    name: &str,
) -> Option<String> {
    let body = resolver?.resolve(name).await?;
    Some(skill_section(name, &body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_traversal_and_separators() {
        assert!(is_valid_skill_name("code-review"));
        assert!(is_valid_skill_name("add_show"));
        assert!(!is_valid_skill_name(""));
        assert!(!is_valid_skill_name(".."));
        assert!(!is_valid_skill_name("../../etc/passwd"));
        assert!(!is_valid_skill_name("a/b"));
        assert!(!is_valid_skill_name("a\\b"));
        assert!(!is_valid_skill_name("a b"));
        assert!(!is_valid_skill_name(&"a".repeat(129)));
    }

    #[test]
    fn strips_frontmatter_only_when_well_formed() {
        assert_eq!(
            strip_frontmatter("---\nname: x\ndescription: y\n---\nBody here\n"),
            "Body here\n"
        );
        // No frontmatter → untouched.
        assert_eq!(strip_frontmatter("Body here\n"), "Body here\n");
        // Unterminated → untouched (don't swallow the file).
        assert_eq!(strip_frontmatter("---\nname: x\n"), "---\nname: x\n");
        // A `---` mid-body (a markdown rule) after real frontmatter still closes
        // at the FIRST fence, which is the frontmatter's.
        assert_eq!(
            strip_frontmatter("---\nname: x\n---\nintro\n\n---\n\nmore\n"),
            "intro\n\n---\n\nmore\n"
        );
    }

    #[tokio::test]
    async fn dir_resolver_reads_first_matching_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let high = tmp.path().join("high");
        let low = tmp.path().join("low");
        for (root, body) in [(&high, "HIGH BODY"), (&low, "LOW BODY")] {
            let dir = root.join("greet");
            std::fs::create_dir_all(&dir).expect("mkdir");
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: greet\n---\n{body}\n"),
            )
            .expect("write");
        }
        let resolver = DirSkillResolver::new(vec![high.clone(), low.clone()]);
        assert_eq!(
            resolver.resolve("greet").await.as_deref(),
            Some("HIGH BODY")
        );
        assert_eq!(resolver.resolve("nope").await, None);
        // Traversal can't escape the root even if a file exists above it.
        assert_eq!(resolver.resolve("../low/greet").await, None);

        // Low root alone falls through to its own copy.
        let resolver = DirSkillResolver::new(vec![tmp.path().join("missing"), low]);
        assert_eq!(resolver.resolve("greet").await.as_deref(), Some("LOW BODY"));
    }

    #[test]
    fn path_list_parsing_is_off_when_empty() {
        assert!(DirSkillResolver::from_path_list("").is_none());
        assert!(DirSkillResolver::from_path_list("  : ").is_none());
        let r = DirSkillResolver::from_path_list("/a: /b :").expect("roots");
        assert_eq!(r.roots, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
    }

    #[tokio::test]
    async fn resolve_section_composes_and_reports_unknown() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("review");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("SKILL.md"), "Check the diff.").expect("write");

        let resolver: Arc<dyn SkillResolver> =
            Arc::new(DirSkillResolver::new(vec![tmp.path().to_path_buf()]));
        let section = resolve_section(Some(&resolver), "review")
            .await
            .expect("section");
        assert!(section.starts_with("## Skill: review\n"));
        assert!(section.ends_with("Check the diff."));

        assert!(resolve_section(Some(&resolver), "unknown").await.is_none());
        // No resolver installed ⇒ every skill is unknown.
        assert!(resolve_section(None, "review").await.is_none());
    }
}
