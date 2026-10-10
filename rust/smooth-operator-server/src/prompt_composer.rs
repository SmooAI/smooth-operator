//! **SEAM — prompt composition.** How the turn's system prompt is assembled
//! from its ordered sections (SMOODEV-3798).
//!
//! The runner used to hard-code the order `base → first-turn greeting →
//! workflow step → skill → suggested-replies trailer`, so a host could only
//! touch `base` (through agent instructions, the per-org persona or the
//! default persona). A host that must put something LAST on every turn — a
//! platform safety block that no agent prompt, greeting or workflow step can
//! override by recency — or that renders its own sections (a caller-context
//! block, a reordered builder) had no place to do it, and an agent with no
//! instructions never reached a host fold at all.
//!
//! A [`PromptComposer`] gets every section, plus who is asking and which
//! agent answers, and returns the ordered sections. The runner drops empty
//! ones and joins the rest with a blank line ([`join_sections`]). With no
//! composer installed the runner uses [`DefaultPromptComposer`], which keeps
//! the historical order.

use smooth_operator::access_control::AccessContext;
use smooth_operator::agent_config::AgentBehaviorConfig;
use std::sync::Arc;

/// Where [`PromptSections::base`] came from, so a composer can tell a real agent
/// prompt from a fallback persona.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BaseSource {
    /// The answering agent's own instructions (+ personality).
    Agent,
    /// The org's saved persona override (`AgentSettings::persona`).
    OrgPersona,
    /// The host's installed default persona (`AppState::default_persona`).
    DefaultPersona,
    /// The server's built-in prompt (no agent, org or host persona).
    #[default]
    BuiltIn,
}

/// Every input to one turn's system prompt. Sections are already rendered;
/// `None` means the section does not apply to this turn.
#[derive(Clone, Copy, Debug)]
pub struct PromptSections<'a> {
    /// The resolved base prompt (agent instructions, org persona, host default
    /// persona or the built-in prompt — see [`base_source`](Self::base_source)).
    pub base: &'a str,
    /// Where [`base`](Self::base) came from.
    pub base_source: BaseSource,
    /// The agent's greeting section — `Some` only on the conversation's first turn.
    pub greeting: Option<&'a str>,
    /// The current conversation-workflow step section, when the agent has a workflow.
    pub workflow: Option<&'a str>,
    /// The turn's invoked skill section.
    pub skill: Option<&'a str>,
    /// The suggested-replies trailer contract (always present).
    pub suggested_replies: &'a str,
    /// The answering agent's resolved config (visibility, workflow, …), when the
    /// session names an agent that resolved.
    pub agent: Option<&'a AgentBehaviorConfig>,
    /// Who is asking: requester user id, groups, org and agent ids.
    pub access: &'a AccessContext,
    /// Whether the session's identity is verified (a prior successful OTP).
    pub session_authenticated: bool,
    /// The conversation this turn belongs to.
    pub conversation_id: &'a str,
}

/// Assembles the turn's system prompt from its sections. Return the sections
/// in order; empty strings are dropped and the rest joined with `"\n\n"`.
pub trait PromptComposer: Send + Sync {
    /// The ordered sections of this turn's system prompt.
    fn compose(&self, sections: &PromptSections<'_>) -> Vec<String>;
}

/// The historical order: base, first-turn greeting, workflow step, skill, then
/// the suggested-replies trailer. Hosts that only append (e.g. a final safety
/// block) can call [`DefaultPromptComposer::sections`] and push onto it.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultPromptComposer;

impl DefaultPromptComposer {
    /// The default ordered sections, for hosts that extend rather than reorder.
    #[must_use]
    pub fn sections(sections: &PromptSections<'_>) -> Vec<String> {
        let mut out = vec![sections.base.to_string()];
        out.extend(sections.greeting.map(str::to_string));
        out.extend(sections.workflow.map(str::to_string));
        out.extend(sections.skill.map(str::to_string));
        out.push(sections.suggested_replies.to_string());
        out
    }
}

impl PromptComposer for DefaultPromptComposer {
    fn compose(&self, sections: &PromptSections<'_>) -> Vec<String> {
        Self::sections(sections)
    }
}

/// Join composed sections: blank (whitespace-only) sections are dropped, the
/// rest joined with a blank line.
#[must_use]
pub fn join_sections(sections: Vec<String>) -> String {
    sections
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Per-turn prompt inputs the handler resolves and the runner composes with.
/// `Default` = a bare turn: built-in base, no agent, unverified session, the
/// default composer.
#[derive(Clone, Default)]
pub struct TurnPrompt {
    /// Where the turn's `system_prompt` came from.
    pub base_source: BaseSource,
    /// The answering agent's resolved config, when any.
    pub agent: Option<AgentBehaviorConfig>,
    /// Whether the session's identity is verified.
    pub session_authenticated: bool,
    /// The host composer (`AppState::prompt_composer`); `None` ⇒ [`DefaultPromptComposer`].
    pub composer: Option<Arc<dyn PromptComposer>>,
}

impl TurnPrompt {
    /// Compose `sections` with the installed composer (or the default) and join.
    #[must_use]
    pub fn render(&self, sections: &PromptSections<'_>) -> String {
        let composed = match &self.composer {
            Some(c) => c.compose(sections),
            None => DefaultPromptComposer.compose(sections),
        };
        join_sections(composed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sections<'a>(access: &'a AccessContext) -> PromptSections<'a> {
        PromptSections {
            base: "BASE",
            base_source: BaseSource::Agent,
            greeting: Some("GREETING"),
            workflow: Some("WORKFLOW"),
            skill: Some("SKILL"),
            suggested_replies: "TRAILER",
            agent: None,
            access,
            session_authenticated: false,
            conversation_id: "c1",
        }
    }

    #[test]
    fn default_composer_keeps_the_historical_order() {
        let access = AccessContext::anonymous();
        let out = TurnPrompt::default().render(&sections(&access));
        assert_eq!(out, "BASE\n\nGREETING\n\nWORKFLOW\n\nSKILL\n\nTRAILER");
    }

    #[test]
    fn absent_sections_are_skipped() {
        let access = AccessContext::anonymous();
        let mut s = sections(&access);
        s.greeting = None;
        s.workflow = None;
        s.skill = None;
        assert_eq!(TurnPrompt::default().render(&s), "BASE\n\nTRAILER");
    }

    #[test]
    fn join_drops_blank_sections() {
        let joined = join_sections(vec!["a".into(), String::new(), "  ".into(), "b".into()]);
        assert_eq!(joined, "a\n\nb");
    }

    /// A host composer can append a final section that trails everything —
    /// the platform-safety use case — and sees who is asking.
    #[test]
    fn host_composer_appends_last_and_sees_the_context() {
        struct Safety;
        impl PromptComposer for Safety {
            fn compose(&self, s: &PromptSections<'_>) -> Vec<String> {
                let mut out = DefaultPromptComposer::sections(s);
                out.push(format!(
                    "SAFETY(source={:?}, authed={}, conv={})",
                    s.base_source, s.session_authenticated, s.conversation_id
                ));
                out
            }
        }
        let access = AccessContext::anonymous();
        let prompt = TurnPrompt {
            composer: Some(Arc::new(Safety)),
            ..TurnPrompt::default()
        };
        let out = prompt.render(&sections(&access));
        assert!(out.starts_with("BASE\n\nGREETING"));
        assert!(out.ends_with("TRAILER\n\nSAFETY(source=Agent, authed=false, conv=c1)"));
    }

    /// A host composer may reorder (the AgentBuilder's ordered sections).
    #[test]
    fn host_composer_may_reorder() {
        struct Reorder;
        impl PromptComposer for Reorder {
            fn compose(&self, s: &PromptSections<'_>) -> Vec<String> {
                vec![
                    s.base.to_string(),
                    s.workflow.unwrap_or_default().to_string(),
                    s.suggested_replies.to_string(),
                ]
            }
        }
        let access = AccessContext::anonymous();
        let prompt = TurnPrompt {
            composer: Some(Arc::new(Reorder)),
            ..TurnPrompt::default()
        };
        assert_eq!(
            prompt.render(&sections(&access)),
            "BASE\n\nWORKFLOW\n\nTRAILER"
        );
    }
}
