//! Minimal `AgentRuntime` that proves smooth-agent consumes smooth-operator.
//!
//! This is the seam where the smooai monorepo's LangGraph pipeline gets
//! re-expressed as a smooth-operator [`Workflow`] / [`Agent`] (see
//! `docs/ARCHITECTURE.md` §2). It does not perform real inference — it
//! constructs the engine's primitives so the wiring is compile-checked and
//! exercised by tests. Real inference arrives in roadmap Phase 3.

use std::sync::Arc;

use anyhow::Result;
use smooth_operator::{
    Agent, AgentConfig, FnNode, LlmConfig, ToolRegistry, Workflow, WorkflowBuilder,
};

use crate::adapter::StorageAdapter;

/// State threaded through the reference workflow: the user's message in, the
/// agent's reply out. Mirrors (in miniature) the LangGraph `StateGraph` state.
#[derive(Debug, Clone, Default)]
pub struct TurnState {
    pub user_message: String,
    pub reply: Option<String>,
}

/// A minimal runtime that owns a constructed smooth-operator [`Agent`] and a
/// trivial single-node [`Workflow`]. Both are real engine objects.
pub struct AgentRuntime {
    agent: Agent,
    workflow: Workflow<TurnState>,
}

impl AgentRuntime {
    /// Construct the runtime from an [`LlmConfig`] and a [`ToolRegistry`].
    ///
    /// This is the load-bearing proof of consumption: it builds an
    /// `AgentConfig` + `Agent` from the engine, and compiles a one-`FnNode`
    /// `Workflow` whose node echoes the user message back as the reply.
    ///
    /// # Errors
    /// Returns an error if the workflow fails to build (misconfigured graph).
    pub fn new(name: impl Into<String>, llm: LlmConfig, tools: ToolRegistry) -> Result<Self> {
        let name = name.into();

        // --- construct a real smooth-operator Agent ---
        let config = AgentConfig::new(&name, "You are a smooth-agent reference runtime.", llm)
            .with_max_iterations(8);
        let agent = Agent::new(config, tools);

        // --- construct a real smooth-operator Workflow with one FnNode ---
        let respond = FnNode::new("respond", |mut state: TurnState| {
            Box::pin(async move {
                state.reply = Some(format!("ack: {}", state.user_message));
                Ok(state)
            })
        });
        let workflow = WorkflowBuilder::new()
            .add_node(respond)
            .set_entry("respond")
            .set_end("respond")
            .build()?;

        Ok(Self { agent, workflow })
    }

    /// Construct a runtime and wire the storage adapter's checkpoint store +
    /// knowledge base into the engine, demonstrating the `StorageAdapter`
    /// accessors plug straight into smooth-operator.
    ///
    /// # Errors
    /// Returns an error if the workflow fails to build.
    pub fn with_storage(
        name: impl Into<String>,
        llm: LlmConfig,
        tools: ToolRegistry,
        storage: &dyn StorageAdapter,
    ) -> Result<Self> {
        let name = name.into();

        let config = AgentConfig::new(&name, "You are a smooth-agent reference runtime.", llm)
            .with_max_iterations(8)
            // KnowledgeBase from the adapter plugs straight into AgentConfig.
            .with_knowledge(storage.knowledge());

        // CheckpointStore from the adapter plugs straight into the Agent.
        let agent = Agent::new(config, tools).with_checkpoint_store(storage.checkpoints());

        let respond = FnNode::new("respond", |mut state: TurnState| {
            Box::pin(async move {
                state.reply = Some(format!("ack: {}", state.user_message));
                Ok(state)
            })
        });
        let workflow = WorkflowBuilder::new()
            .add_node(respond)
            .set_entry("respond")
            .set_end("respond")
            .build()?;

        Ok(Self { agent, workflow })
    }

    /// The engine-generated agent id (proves the `Agent` was constructed).
    pub fn agent_id(&self) -> &str {
        &self.agent.id
    }

    /// Run one turn through the smooth-operator workflow. Returns the reply
    /// produced by the node. (No LLM call — the node is deterministic.)
    ///
    /// # Errors
    /// Returns an error if the workflow run fails.
    pub async fn run(&self, message: impl Into<String>) -> Result<String> {
        let state = TurnState {
            user_message: message.into(),
            reply: None,
        };
        let out = self.workflow.run(state).await?;
        Ok(out.reply.unwrap_or_default())
    }

    /// Borrow the underlying engine agent (e.g. to attach an event handler).
    pub fn agent(&self) -> &Agent {
        &self.agent
    }
}

/// Convenience: an `Arc`-wrapped runtime.
pub type SharedRuntime = Arc<AgentRuntime>;

#[cfg(test)]
mod tests {
    use super::*;

    fn test_llm() -> LlmConfig {
        LlmConfig::openrouter("test-key").with_model("openai/gpt-4o")
    }

    #[tokio::test]
    async fn runtime_constructs_agent_and_runs_workflow() {
        let rt =
            AgentRuntime::new("ref-agent", test_llm(), ToolRegistry::new()).expect("build runtime");
        // The Agent was really constructed — it has an engine-assigned id.
        assert!(!rt.agent_id().is_empty());
        // The Workflow really ran through its FnNode.
        let reply = rt.run("hello world").await.expect("run");
        assert_eq!(reply, "ack: hello world");
    }
}
